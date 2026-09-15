# chaoticdb

一个用**纯 Rust + tokio** 从零手写的教学型单机关系数据库。按 chibicc 的增量迭代风格演化：
每个能力点先写失败测试（红），再做最小实现（绿），提交保持细粒度。不使用任何数据库库——
存储、索引、事务、执行器、网络前端全部手写。

它由 chibidb 演进而来，做了一批有意的结构调整（见「与 chibidb 的差异」）。

## 快速开始

```powershell
cargo run -q                    # 内存实例 REPL（临时目录后端，退出自动清理）
cargo run -q -- <dir>           # 文件实例 REPL（多库数据根目录）
cargo run -q -- serve <dir> [addr]   # TCP server，addr 缺省取 config.toml 的 server.addr
cargo run -q -- client [addr]   # 连接 server 的交互式客户端
cargo test                      # 全量回归（698 passed + 9 个 #[ignore] 性能探针）
```

REPL / client 中输入 `exit` 或 `quit` 退出。启动时从**当前工作目录**读取 `config.toml`
（缺失则全部使用默认值）；可复制 `config.example.toml` 作为起点，其中含各条目说明与默认值。
页大小 `PAGE_SIZE = 8192` 是编译期常量，不是配置项。

已生效的配置条目：

- `storage.default_engine`（`heap` / `lsm`）、`storage.page_layout`（`row` / `pax`，仅 heap）
- `storage.buffer_pool_frames`、`storage.eviction`（`lru` 默认 / `clock` / `fifo`）
- `storage.double_write`、`storage.inline_lob_limit`、`storage.lsm_compaction_trigger`
- `wal.checkpoint_threshold`
- `server.addr`、`server.http_addr`（可选）、`server.mysql_addr`（可选）、
  `server.web_root`（默认 `"web/dist"`）、`server.admin_api`（默认 `false`）、
  `server.thread_model` / `server.worker_threads`
- `execution.mode`（`chunk` 默认 / `volcano`）
- `auth.enabled`
- `transaction.isolation`（`read_committed` 默认 / `repeatable_read` / `serializable`）、
  `transaction.lock_timeout_ms`
- `web.enabled` / `web.title` / `web.page_preview`（内置演示前端，见下）

配置为严格模式（`deny_unknown_fields`）：出现未知条目会直接报错。

### 前端

- **Text TCP**：`server.addr`；`[u32 len][sql]` 请求 / 帧响应，`client` 子命令即用它。
- **HTTP/JSON**：设置 `server.http_addr` 后，`GET /health` 返回 `{"status":"ok"}`，
  `POST /query` 以 `{"sql":"..."}` 请求、返回 `{"results":[...]}`；`GET /session` 领取会话，
  之后携带 `X-Chibi-Session` 头即可让事务跨请求。同一监听器在 `[web] enabled = true` 时托管
  内置演示控制台，否则托管 `server.web_root` 指定的静态站点（默认 `web/dist`）。`/api/*`
  在 `[web] enabled` 或 `server.admin_api` 开启后可用（提供 `/api/parse`、`/api/plan`、
  `/api/config`、`/api/schema`、`/api/buffer`，以及 `web.page_preview` 门控的
  `/api/files`、`/api/page`）。
- **MySQL wire**：设置 `server.mysql_addr` 后，可用 `mysql` 客户端连接：握手使用
  `mysql_native_password`，`COM_QUERY` 走文本结果集，并支持
  `COM_STMT_PREPARE`/`EXECUTE`/`CLOSE`/`RESET`（预处理语句）。

### 内置演示前端

设置 `[web] enabled = true` 后，HTTP 监听器在 `/` 直接托管一个**无需构建、零依赖**的演示控制台
（源码在 `web/demo/`，编译期由 `include_str!` 内嵌；`web.enabled` 为真时优先于 `server.web_root`）：

- **SQL 控制台**：执行 SQL 并展示结果表 / affected / 错误；会话可跨请求（事务）。
- **编译流水线**：输入 → Token 流 → Statement(AST) → LogicalOperator（逻辑计划）→ 优化 → PhysicalOperator（缩进算子树）。
- **存储与页**：浏览数据目录文件、页导航、文件头 / 槽目录 / B+ 节点结构 + Hex / ASCII。
- **BufferPool**：各库缓冲池的命中率、命中 / 未命中、淘汰（clean / dirty）与驻留帧 / 容量。
- **Schema 树**：库 / 表 / 列与约束。

相关接口（开启 `[web] enabled` **或** `server.admin_api` 后可用）：`POST /api/plan`、`GET /api/config`、
`GET /api/schema`、`GET /api/buffer`、`GET /api/files`、`GET /api/page`；后两个另需 `[web] page_preview = true`
（会读取原始数据页）。

```toml
[web]
enabled = true          # 在 / 托管内置演示控制台
title = "chaoticdb console"
page_preview = false    # 允许 /api/files 与 /api/page 预览磁盘页
```

### 冒烟演示

```powershell
python scripts\smoke.py     # POSIX: python3 scripts/smoke.py
```

脚本会构建调试二进制、启动 server、通过 client 建表/插入/建索引/查询，随后**强杀**
server 进程（模拟崩溃），再重新打开同一数据目录，验证 WAL 崩溃恢复：已提交数据全部还在，
成功时打印 `SMOKE OK` 并以 0 退出。

## 支持的 SQL

```sql
-- 库（单实例多库；chibi_meta 为系统保留目录）
CREATE DATABASE shop;  DROP DATABASE shop;  USE shop;
-- 元数据（虚拟只读库：schemata / tables / columns，仅 SELECT/EXPLAIN）
USE information_schema;
SELECT table_name, engine, page_layout FROM tables WHERE table_schema = 'shop';
-- 元命令（只读；结果列名为引擎风格小写）
SHOW TABLES;  SHOW DATABASES;  SHOW COLUMNS FROM t;  SHOW COLUMNS IN t;  DESCRIBE t;
-- 用户（存于 chibi_meta 系统库，口令加盐 SHA-256；另有 MySQL 原生校验子）
CREATE USER alice IDENTIFIED BY 'secret';  DROP USER alice;
-- 认证（仅当 auth.enabled = true；登录绑定会话）
LOGIN alice IDENTIFIED BY 'secret';
-- 权限（read/write；ON * 表示所有库）
GRANT READ, WRITE ON shop TO alice;  GRANT ALL ON * TO alice;  REVOKE WRITE ON shop FROM alice;
-- DDL（可选 ENGINE = heap|lsm、PAGE_LAYOUT = row|pax；缺省取 storage 配置）
CREATE TABLE t (id int primary key, name char(10) not null,
                score float default 0, email char(20) unique) ENGINE = lsm;
CREATE INDEX idx_name ON t (col);
DROP INDEX idx_name;              -- 约束索引（PK/UNIQUE）拒绝 DROP
DROP TABLE t;
CREATE VIEW v AS SELECT id, score FROM t WHERE score >= 75.0;
DROP VIEW v;
-- DML
INSERT INTO t VALUES (1,'a',1.5),(2,'b',2.0);   -- 多值行，负号与 null 关键字
INSERT INTO t (name, id) VALUES ('a', 1);        -- 列清单，省略列取 DEFAULT
UPDATE t SET score = (SELECT max(score) FROM other) WHERE id < 10;
DELETE FROM t WHERE id IN (SELECT id FROM other);
-- 查询
SELECT [DISTINCT] * | expr [AS alias] (, ...)
  [UNION [ALL] SELECT ...]*    -- 并集，尾部 ORDER BY/LIMIT 管全体
  FROM tref | view (, ...)*    -- 逗号 = cross join
  [JOIN | LEFT [OUTER] | RIGHT [OUTER] JOIN tref ON cond]*   -- INNER / LEFT / RIGHT
  [WHERE expr] [GROUP BY expr (, ...)*] [HAVING expr]
  [ORDER BY expr [ASC|DESC] (, ...)*] [LIMIT n [OFFSET m]]
-- 聚合：count(*)/count(x)/sum/avg/min/max（均支持 DISTINCT expr）；空集 sum/avg/min/max → NULL
-- 表达式：+ - * / %（整数取模/浮点 fmod，模零报错）、and/or/not（三值逻辑）、
--          比较、is [not] null、括号、
--          expr [NOT] LIKE 'pattern' [ESCAPE 'c']（% 任意串、_ 单字符，区分大小写）
--          字符串函数：concat / upper / lower / length / substring(s, start[, len])（substr 为别名）
--          expr [NOT] IN (值列表)、expr [NOT] IN (SELECT ...)（单列，可相关）
--          [NOT] EXISTS (SELECT ...)、标量 (SELECT ...)（可用于比较与算术）
--          子查询可引用外层列（相关子查询，支持多层）
-- 事务
BEGIN;  START TRANSACTION [READ ONLY|READ WRITE|WITH CONSISTENT SNAPSHOT];  COMMIT;  ROLLBACK;
CHECKPOINT;                    -- 刷盘 + 截断日志（开事务时拒绝）
VACUUM;                        -- 物理回收已提交删除的行与孤版本（开事务时拒绝）
EXPLAIN SELECT ...;            -- 输出 FullScan / IndexScan / OrderedIndexScan / HashJoin ...
```

语义要点（由测试锁定）：

- 标识符大小写不敏感；`NULL` 遵循 SQL 三值逻辑（NULL 比较为 UNKNOWN，WHERE 只放行 TRUE）。
- `IN (值列表)` 与 `IN (子查询)` 都遵循三值语义：含 NULL 时 `NOT IN` 永不返回 TRUE。
- 整数除法向零截断；除零、模零报错；`%` 对整数取模、对浮点用 fmod。
- `date` 严格按 `YYYY-MM-DD` 校验（闰年正确），与字符串比较时隐式转换。
- `char(n)` 按字符数校验；`text` 无长度限制，但单行记录超页报错；超长字符串体外存为 LOB 引用。
- 列约束：`primary key` 隐含 `not null` + `unique`；PK/UNIQUE 自动建唯一索引，违反报 `duplicate key`；
  UNIQUE 允许多个 NULL；`default` 在建表时定型，`INSERT (列清单)` 省略列取默认值。
- `DISTINCT` 对投影结果去重（NULL 彼此相等）；`LEFT`/`RIGHT JOIN` 保留未匹配侧并补 NULL。
- `LIKE` 的 `%`/`_` 通配、区分大小写，可用 `ESCAPE` 指定转义字符；任一侧为 NULL 时结果为 NULL。
- `concat` 把任意标量转文本拼接（任一参数 NULL 则结果 NULL）；`upper`/`lower`/`length`/`substring`
  仅接受字符串，NULL 传播；`substring` 下标从 1 起，越界得空串。
- 显式事务内执行 DDL 报错；`CHECKPOINT`/`VACUUM` 在任一事务未结束时报错。
- `information_schema` 只读；`SHOW COLUMNS`/`DESCRIBE` 只描述表，视图报错；`SHOW TABLES` 列出表与视图并排序。
- REPL/client 仅在 stdin 为终端时打印提示符，管道输入不污染首行、表格保持对齐。

## 架构（SQL 的一生）

```
SQL 字符串
  → lexer            分词（int/float/str/标识符/标点/注释）
  → parser           手写递归下降 → AST（sql::ast::Stmt/Expr）
  → Instance         多库路由 / 用户 / 权限 / information_schema（instance.rs）
  → Database         表存在性校验、调用规划器、执行物理算子（db/mod.rs::resolve_optimize_execute）
  → planner          常量折叠 → LogicalOperator → 优化（谓词下推、WHERE→JOIN）→ lower 成 PhysicalOperator
                    （exec/planner/；访问路径选择、`*`/别名展开、聚合重写都在此）
  → operator         火山算子（exec/operator/，对 Statement 无知）
                    （过滤/投影/聚合/分组/排序/limit/连接/索引扫描/视图/子查询）
  → chunk/volcano    列式批处理（CHUNK_ROWS=1024）或行式火山，两模式结果一致
  → index            保序字节键 B+ 树（[key,rid] 复合键、分裂/借用/合并、范围扫描）
  → txn              PG 式快照 {xmax,xip} + clog 提交位图 + 行级锁 + SSI
  → WAL              提交时 fsync；崩溃后只重放有 COMMIT 的事务
  → storage          TableStorage 接缝：HeapEngine（slotted/PAX 页）或 LsmEngine
  → BufferPool       8KB 帧、可配置淘汰（lru/clock/fifo）、pin 引用计数、脏页写回
  → DiskManager      分页文件 IO（按文件加锁），可选 Double-Write Buffer
```

语句分类不再用自由函数，而是挂在 AST 上：

- `Stmt::is_read_only()`：只读语句（SELECT/EXPLAIN/SHOW），自动提交时无需事务簿记。
- `Stmt::needs_exclusive()`：DDL/CHECKPOINT/VACUUM 需要数据库独占；DML 与事务控制共享数据库锁，
  同库并发写按行由锁管理器串行。

### 源码结构

```
src/
  lib.rs  main.rs  error.rs  config.rs  wal.rs  instance.rs  value.rs
  sql/       lexer parser ast result datetime
  exec/         mod command dml eval aggregate subquery chunk
  exec/planner/ mod logical lower access fold explain util
  exec/operator/ mod basic scan index_scan join subquery
  storage/   page disk header dwb buffer replacer slotted engine heap codec lob pax lsm/*
  index/     key node btree
  catalog/   mod meta
  txn/       clog lock ssi transaction trx
  db/        mod store recovery checkpoint unique
  net/       server client protocol wire repl render http admin json session mysql
ffi/         C ABI（cdylib：chaoticdb_open/exec/query/free/close/last_error）
```

## 存储、文件布局与多库

`Instance`（`instance.rs`）是一个数据根目录，内含多个命名数据库（每个一个子目录），
外加系统保留目录 `chibi_meta`（存 `databases`/`users`/`privileges`）与虚拟只读库
`information_schema`。每个数据库一把 `RwLock<Database>`：不同库可并发使用，库内只读共享、
写语句按行串行。

表存储通过 `TableStorage` 抽象（`Arc<dyn TableStorage>` 存于 catalog），执行层、回滚、
vacuum、唯一性检查与 WAL 重放都走该接缝。两种引擎：

- **Heap**（默认）：`HeapEngine` + slotted page（或 PAX 列式页）+ B+ 树索引。
- **LSM**：`MemTable`（有序 + 墓碑）→ `SSTable`（数据块 + 索引块 + bloom + footer）→
  `LsmStore`（分级压实）→ `PersistentLsm`（`MANIFEST` 原子提交、重开恢复）；`LsmEngine` 以内部
  行号键接入 `TableStorage`。

单个数据库目录的文件布局：

```
catalog.bin            # 元数据 + 事务簿记 + 视图定义（临时文件 fsync 后 rename 原子替换）
wal.bin                # 预写日志（干净 checkpoint 后清空）
tables/000000.dbf ...  # 堆表文件
tables/000000.lsm/     # LSM 表：MANIFEST + SSTable
indexes/000000.idxf ...# 每索引一棵 B+ 树
lobs/<id>.lob          # 外存大对象
dwb.bin                # Double-Write Buffer（storage.double_write 开启时）
```

`FileId` 只有一个类型：表文件的持久 catalog/WAL 编号与缓冲池句柄是同一个值；索引文件在缓冲池里
带上高位 tag（`INDEX_FILE_TAG`）以免与表文件号冲突，低比特仍是落盘用的索引编号。

## 并发与事务

详见 [`MVCC.md`](MVCC.md)。要点：

- **快照隔离**：PG 式快照 `{xmax, xip}`，可见性查共享的 clog 提交位图（`xid < xmax`、
  不在 `xip`、clog 已提交）。每行带 creator/deleter 事务号与 `next_rid` 版本链前向指针。
- **行级锁**：写前对 `(table, rid)` 加锁，事务结束释放；不同行并发、同行排队；等待超时报
  `lock wait timeout`，死锁检测回退。UNIQUE/PK 检查按键加锁，理论上并发下成立。
- **隔离级别**（`transaction.isolation`）：
  - `read_committed`（默认）：每条语句取新快照；等锁后发现目标行被并发提交改动则重启该语句（EPQ）。
  - `repeatable_read`：事务内固定快照；写写冲突在提交时报 `40001`。
  - `serializable`：固定快照 + SSI；跟踪 rw-反依赖，提交时检测到环则报 `40001`。
- **崩溃恢复**：WAL 只重放有 COMMIT 记录的事务，重放幂等；索引页属派生数据，重放后按堆重建。
- **空间回收**：`VACUUM` 物理回收已提交删除标记的行、崩溃事务遗留的孤儿版本与 stale 索引项，
  并推进 clog 地平线使 xid 状态有界；日志超预算且无开事务时自动 checkpoint。

## 测试

`cargo test --workspace` 当前 **698 passed + 9 ignored**。集成测试覆盖词法/语法/求值/
LIKE/字符串函数/聚合/连接/子查询（含相关）/UNION/表约束（PK/UNIQUE/NOT NULL/DEFAULT）/
索引/持久化/事务/WAL 恢复/vacuum/存储层/网络协议等。`tests/miniob_compat.rs` 用经典
student/course/sc 场景做端到端回归；`tests/engine_equivalence.rs` 用确定性随机脚本对
heap/LSM 两引擎做差分等价（含中途重开）；`tests/concurrency_fuzz.rs` 用固定种子的随机并发
调度断言余额守恒与主键唯一。

`tests/bench.rs`（6 个）与 `tests/perf_stats.rs`（3 个）是 `#[ignore]` 性能探针，例如：

```text
cargo test --release --test bench -- --ignored --nocapture
cargo test --test perf_stats -- --ignored --nocapture
```

`tests/wal.rs` 等崩溃用例用 `Database::simulate_crash()` 跳过缓冲池的干净落盘来模拟进程被杀。

## 与 chibidb 的差异

- 事务相关模块统一收进 **`txn/`**（`clog`/`lock`/`ssi`/`transaction`/`trx`），不再散落在 `db/`。
- `Database` 拆分为 **`db/*`**：`mod.rs`（门面与语句执行）、`store.rs`（存储/catalog/回滚）、
  `recovery.rs`（WAL 重放与索引重建）、`checkpoint.rs`（flush/vacuum）、`unique.rs`（唯一约束）。
- 语句分类由 **`Stmt::is_read_only()` / `needs_exclusive()`** 决定，取代原来的自由函数。
- 移除了 `Stage` / `Pipeline` 门面：不恢复门面对象；`resolve_optimize_execute` 调用
  `exec/planner`（LogicalOperator → 优化 → lower 成 PhysicalOperator）后执行。
- 规划与算子分层：`exec/operator/` 对 Statement 无知，DML 命令、索引选路、子查询计划都在
  `exec/planner/` 完成；子查询在计划期预降为子计划，执行期不再现场 `plan_select`。
- 表文件使用**单一 `FileId`** 同时作为持久编号与缓冲池句柄（索引文件加高位 tag 区分）。
- HTTP 会话头仍为 `X-Chibi-Session`，系统库仍为 `chibi_meta`（保留兼容命名）。

## 依赖

直接依赖：`tokio`（异步运行时与网络）、`parking_lot`（锁）、`serde`（配置反序列化）、
`toml`（`config.toml`）、`thiserror`（错误）、`sha2`（口令散列）、`tempfile`（内存实例与测试）。
`ffi` 子 crate 把引擎编译成 `cdylib`，供非 Rust 前端与基准脚本通过 C ABI 进程内调用。

