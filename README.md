# chibidb

一个用纯 Rust + tokio 从零手写的教学型单机数据库，对标 OceanBase miniob（2024 版）。
按 chibicc 的增量迭代风格演化：每个能力点先写失败测试（红），再做最小实现（绿），
提交保持细粒度。不使用任何数据库库——存储、索引、事务、执行器全部手写。

## 快速开始

```powershell
cargo run -q                    # 内存实例 REPL（临时目录后端，自动清理）
cargo run -q -- <dir>           # 文件实例 REPL（数据根目录，多库落盘）
cargo run -q -- serve <dir>     # TCP server，默认监听 127.0.0.1:5678
cargo run -q -- client [addr]   # 连接 server 的交互式客户端
# HTTP/JSON 前端：配置 server.http_addr 后，POST /query {"sql":"..."} → {"results":[...]}
# MySQL 前端：配置 server.mysql_addr 后，可用 mysql 客户端连接（mysql_native_password、文本结果集）
cargo test                      # 全量回归（470 tests）
cargo test --release --test bench -- --ignored --nocapture   # 索引 vs 全表扫基准
```

REPL / client 中输入 `exit` 或 `quit` 退出。

启动时从当前目录读取 `config.toml`（缺失则全用默认值）。已生效条目：
`storage.buffer_pool_frames`、`storage.double_write`、`storage.default_engine`（`"heap"` / `"lsm"`）、
`storage.inline_lob_limit`、`storage.lsm_compaction_trigger`、`wal.checkpoint_threshold`、`server.addr`、
`server.http_addr`（可选，HTTP/JSON 监听）、`server.mysql_addr`（可选，MySQL wire 监听）、
`server.thread_model`/`worker_threads`、
`transaction.conflict`（`"fcw"` / `"2pl"`）、`transaction.lock_timeout_ms`；
其余为后续阶段预留（详见 `HANDOFF.md` §10 重构路线图）。

### 冒烟演示

```powershell
scripts\smoke.ps1
```

脚本会启动 server、通过 client 建表/插入/建索引/查询，随后强杀 server 进程
（模拟崩溃）并重新打开数据目录，展示 WAL 崩溃恢复：已提交数据全部还在。

## 支持的 SQL

```sql
-- 库（单实例多库；chibi_meta 为系统保留目录）
CREATE DATABASE shop;  DROP DATABASE shop;  USE shop;
-- 元数据（虚拟只读库：schemata / tables / columns）
USE information_schema;  SELECT table_name, engine FROM tables WHERE table_schema = 'shop';
-- 用户（存于 chibi_meta 系统库，口令加盐 SHA-256）
CREATE USER alice IDENTIFIED BY 'secret';  DROP USER alice;
-- 认证（仅当 auth.enabled = true；登录绑定会话）
LOGIN alice IDENTIFIED BY 'secret';
-- 权限（read/write；ON * 表示所有库；`auth.enabled=true` 时强制：
--       SELECT/EXPLAIN 需 read，DML/DDL 需 write，未登录/越权分别报错）
GRANT READ, WRITE ON shop TO alice;  GRANT ALL ON * TO alice;  REVOKE WRITE ON shop FROM alice;
-- DDL（可选 ENGINE = heap|lsm，缺省取 storage.default_engine）
CREATE TABLE t (id int primary key, name char(10) not null,
                score float default 0, email char(20) unique) ENGINE = lsm;
CREATE INDEX idx_name ON t (col);
DROP INDEX idx_name;              -- 约束索引（PK/UNIQUE）拒绝 DROP
DROP TABLE t;
CREATE VIEW v AS SELECT id, score FROM t WHERE score >= 75.0;
DROP VIEW v;
-- DML
INSERT INTO t VALUES (1,'a',1.5),(2,'b',2.0);   -- 多值行，null 关键字
INSERT INTO t (name, id) VALUES ('a', 1);        -- 列清单，省略列取 DEFAULT
UPDATE t SET score = (SELECT max(score) FROM other) WHERE id < 10;
DELETE FROM t WHERE id IN (SELECT id FROM other);
-- 查询
SELECT [DISTINCT] * | expr [AS alias] (, ...)
  [UNION [ALL] SELECT ...]*    -- 并集，尾部 ORDER BY/LIMIT 管全体
  FROM tref | view (, ...)*    -- 逗号 = cross join
  [JOIN | LEFT [OUTER] | RIGHT [OUTER] JOIN tref ON cond]*   -- INNER / LEFT / RIGHT
  [WHERE expr]
  [GROUP BY expr (, expr)*]
  [HAVING expr]
  [ORDER BY expr [ASC|DESC] (, ...)*]
  [LIMIT n [OFFSET m]]
-- 聚合：count(*)/count(x)/sum/avg/min/max（均支持 DISTINCT expr）；空集 sum/avg/min/max → NULL
-- 表达式：+ - * / %（整数取模/浮点 fmod，模零报错）、and/or/not（三值逻辑）、
--          比较、is [not] null、括号
--          expr [NOT] LIKE 'pattern' [ESCAPE 'c']（% 任意串、_ 单字符，区分大小写）
--          字符串函数：concat / upper / lower / length / substring(s, start[, len])
--          expr [NOT] IN (值列表)、expr [NOT] IN (SELECT ...)（单列，可相关）
--          [NOT] EXISTS (SELECT ...)、标量 (SELECT ...)（可用于比较与算术）
--          子查询可引用外层列（相关子查询，支持多层）
-- 事务
BEGIN; COMMIT; ROLLBACK;
CHECKPOINT;                    -- 刷盘 + 截断日志（开事务时拒绝）
VACUUM;                        -- 物理回收已提交删除的行与失效索引项（开事务时拒绝）
EXPLAIN SELECT ...;            -- 输出 FullScan / IndexScan / NestedLoopJoin
```

语义要点：

- 标识符大小写不敏感；`NULL` 遵循 SQL 三值逻辑（`1/0=2`：NULL 比较为 UNKNOWN，WHERE 只放行 TRUE）
- `IN (值列表)` 与 `IN (子查询)` 均遵循三值语义：列表/子查询含 NULL 时 `NOT IN` 永不返回 TRUE
- `DISTINCT` 对投影结果去重，NULL 彼此相等
- `LEFT JOIN` / `RIGHT JOIN` 保留未匹配的左/右侧行，另一侧补 NULL
- `date` 严格按 `YYYY-MM-DD` 校验（闰年正确）；与字符串比较时隐式转换
- `char(n)` 按字符数校验；`text` 无长度限制但单行超页报错
- 列约束：`primary key` 隐含 `not null`+`unique`；PK/UNIQUE 自动建唯一索引，违反报 `duplicate key`；
  UNIQUE 允许多个 NULL；`default` 在建表时定型，`INSERT (列清单)` 省略列取默认值
- `LIKE` 的 `%`/`_` 通配、区分大小写、无转义字符；任一侧为 NULL 时结果为 NULL
- `concat` 将任意标量转为文本拼接（任一参数 NULL 则结果 NULL）；`upper`/`lower`/`length`/
  `substring` 仅接受字符串，NULL 传播；`substring` 下标从 1 起，缺省长度到串尾
- 整数除法向零截断；除零、模零报错
- ORDER BY 可引用 SELECT 别名；JOIN 中同名非限定列报 ambiguous
- 子查询支持相关（引用外层列，多层嵌套 OK）：按外层行求值并改写为字面量；视图可叠在 JOIN 中、可套视图
- 显式事务内执行 DDL 报错

## 架构（SQL 的一生）

```
SQL 字符串
  → lexer          分词（int/float/str/标识符/标点/注释）
  → parser         手写递归下降 → AST
  → executor       常量求值/过滤/投影/聚合/分组/排序/limit/连接/索引扫描
  → 规则优化器      AND 链提取索引谓词（等值/上下界合并成一段范围扫，EXPLAIN 可观测）
  → 子查询物化      按外层行求值 IN/EXISTS/标量子查询并改写为字面量（支持相关子查询）
  → B+ 树索引      保序字节键、分裂/借用/合并、范围扫描
  → MVCC           快照隔离、undo 回滚、BEGIN/COMMIT/ROLLBACK
  → TransactionManager 事务 id 分配 + committed/open 集合 + 快照
  → WAL            提交时 fsync；崩溃后重放已提交事务
  → slotted page   8KB、槽目录、变长条目
  → TableStorage   表存储接缝（`insert/delete/delete_mark/scan/get`，带 MVCC 语义）
  → HeapEngine     `TableStorage` 的堆实现（Rid 寻址、多页 first-fit）
  → BufferPool     8KB 帧、LRU、脏页写回（元数据锁 + 每帧页闩）
  → DiskManager    分页文件 IO
```

表存储通过 `TableStorage` 抽象（`Arc<dyn TableStorage>` 存于 catalog），执行层与
`Database` 的 `store_*`/回滚/vacuum/唯一性检查都走该接缝。两种引擎：
- **Heap**（默认）：`HeapEngine` + slotted page + B+ 树索引
- **LSM**：`MemTable`（有序 + 墓碑）→ `SSTable`（数据块 + 索引块 + bloom + footer，varint/前缀压缩）
  → `LsmStore`（flush/compaction）→ `PersistentLsm`（`MANIFEST` 原子提交、重开恢复、活跃表达阈值自动合并）；
  `LsmEngine` 以内部行号键接入 `TableStorage`，版本化记录与堆一致

新建表的引擎由 `CREATE TABLE ... ENGINE = heap|lsm` 指定，未写时取 `storage.default_engine`；
catalog 记录每表引擎，打开/建表/删除/WAL 重放均按引擎分派。

文件布局：`catalog.bin`（元数据 + 事务簿记 + 视图定义）、`wal.bin`（预写日志，
干净关闭后清空）、`tables/*.dbf`（堆表文件）与 `tables/*.lsm/`（LSM 表：`MANIFEST` + SSTable）、
`indexes/*.idxf`（每索引一棵 B+ 树）、`lobs/<id>.lob`（外存大对象）。超过 `storage.inline_lob_limit`
的字符串在行编码时外存为 LOB 引用、解码时解析回字符串；版本被物理删除（回滚/vacuum）或表被删除时
回收其 LOB 文件。扫描会做**列剪裁**：未被查询引用的 LOB 列不解码（如 `count(*)`、只投影普通列），
避免读取大对象；`LobReader` 提供分块流式读接口（单值仍整体物化为字符串）。

## 并发与事务

- 快照隔离：事务开始时记录已提交事务快照；每行带 creator/deleter 事务号，
  读时按可见性规则过滤；UPDATE = 删除标记 + 新版本
- 库内读并发：每个数据库一把 `RwLock`，只读语句共享读锁，写语句独占；
  `BufferPool` 用元数据锁 + 每帧页闩，多读互不阻塞；写事务在语句粒度串行
- 冲突策略可配置：`transaction.conflict = "fcw" | "2pl"`。默认 **FCW**
  （first-committer-wins）：提交时比对被改写基版本的 `prev_deleter` 与当前标记，
  若其间有后提交的事务改过同一行则回滚失败方
- **2PL**（pessimistic）：每库一把悲观写锁，显式事务在 `BEGIN`（取快照前）持锁至
  `COMMIT/ROLLBACK`，自动提交写语句在语句内持锁；等待超过
  `transaction.lock_timeout_ms` 报 `lock wait timeout`。写者串行、后写者基于最新提交，
  避免丢失更新；`Instance` 先取写锁再取库锁，规避锁序死锁
- 连接断开自动回滚其未提交事务
- 崩溃恢复：WAL 只重放有 COMMIT 记录的事务，重放幂等（精确 Rid 回写 +
  删除标记条件重放）；索引页属派生数据，恢复时对被触及的表重建
- 空间回收：`VACUUM` 物理删除已提交删除标记的行（含 stale 索引项清理）与
  崩溃事务遗留的孤儿版本；日志超预算且无开事务时自动 checkpoint

## 测试

`cargo test` 跑 470 个测试，覆盖词法/语法/求值/LIKE/字符串函数/聚合/连接/子查询（含相关）/UNION/
表约束（PK/UNIQUE/NOT NULL/DEFAULT）/索引/持久化/事务/WAL 恢复/vacuum/存储层/网络协议等，
另有 `tests/miniob_compat.rs` 用经典 student/course/sc 场景做端到端回归。集成测试的 `with_dbs` 模式让同一用例在内存后端
与文件后端各跑一遍；WAL 测试用 `Database::simulate_crash()` 模拟进程被杀。

## 性能

`tests/bench.rs`（release、5 万行）对比索引访问路径与全表扫：

| 查询 | 索引 | 全表扫 |
|---|---|---|
| 点查 `id = 12345` | ~16 µs | ~29 ms |
| 单边范围 `id < 100` | ~67 µs | ~27 ms |
| 双边范围 `id in [10000,10100)` | ~0.14 ms | ~34 ms |
| 有序范围 `id > 49900 ORDER BY id` | ~0.23 ms | ~24 ms |

要点：单表 SELECT 会在扫描前先选定访问路径，命中索引时完全跳过堆扫描；AND 链里
同一索引列的 `>`/`>=`/`<`/`<=` 会合并为一段 B+ 树范围扫；ORDER BY 恰为索引列升序时
直接复用叶链顺序、跳过排序（EXPLAIN 为 `OrderedIndexScan`）；只读事务不重写 catalog。

## 依赖

仅三个：`tokio`（异步运行时与网络）、`thiserror`、`tempfile`（测试）。
