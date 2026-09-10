# chibidb

一个用纯 Rust + tokio 从零手写的教学型单机数据库，对标 OceanBase miniob（2024 版）。
按 chibicc 的增量迭代风格演化：每个能力点先写失败测试（红），再做最小实现（绿），
提交保持细粒度。不使用任何数据库库——存储、索引、事务、执行器全部手写。

## 快速开始

```powershell
cargo run -q                    # 内存数据库 REPL（临时目录后端，自动清理）
cargo run -q -- <dir>           # 文件数据库 REPL（数据落盘，重启不丢）
cargo run -q -- serve <dir>     # TCP server，默认监听 127.0.0.1:5678
cargo run -q -- client [addr]   # 连接 server 的交互式客户端
cargo test                      # 全量回归（231 tests）
```

REPL / client 中输入 `exit` 或 `quit` 退出。

### 冒烟演示

```powershell
scripts\smoke.ps1
```

脚本会启动 server、通过 client 建表/插入/建索引/查询，随后强杀 server 进程
（模拟崩溃）并重新打开数据目录，展示 WAL 崩溃恢复：已提交数据全部还在。

## 支持的 SQL

```sql
-- DDL
CREATE TABLE t (id int, name char(10), score float, d date, body text);
CREATE INDEX idx_name ON t (col);
DROP INDEX idx_name;
DROP TABLE t;
CREATE VIEW v AS SELECT id, score FROM t WHERE score >= 75.0;
DROP VIEW v;
-- DML
INSERT INTO t VALUES (1,'a',1.5),(2,'b',2.0);   -- 多值行，null 关键字
UPDATE t SET score = score + 1 WHERE id < 10;
DELETE FROM t WHERE name IS NULL;
-- 查询
SELECT [DISTINCT 未实现] * | expr [AS alias] (, ...)
  FROM tref | view (, ...)*    -- 逗号 = cross join
  [JOIN tref ON cond]*         -- 仅 INNER
  [WHERE expr]
  [GROUP BY expr (, expr)*]
  [HAVING expr]
  [ORDER BY expr [ASC|DESC] (, ...)*]
  [LIMIT n [OFFSET m]]
-- 聚合：count(*)/count(x)/sum/avg/min/max；空集 sum/avg/min/max → NULL
-- 表达式：+ - * /、and/or/not（三值逻辑）、比较、is [not] null、括号
--          expr [NOT] IN (值列表)、expr [NOT] IN (SELECT ...)（单列）
--          [NOT] EXISTS (SELECT ...)、标量 (SELECT ...)（可用于比较与算术）
-- 事务
BEGIN; COMMIT; ROLLBACK;
EXPLAIN SELECT ...;            -- 输出 FullScan / IndexScan / NestedLoopJoin
```

语义要点：

- 标识符大小写不敏感；`NULL` 遵循 SQL 三值逻辑（`1/0=2`：NULL 比较为 UNKNOWN，WHERE 只放行 TRUE）
- `IN (值列表)` 与 `IN (子查询)` 均遵循三值语义：列表/子查询含 NULL 时 `NOT IN` 永不返回 TRUE
- `date` 严格按 `YYYY-MM-DD` 校验（闰年正确）；与字符串比较时隐式转换
- `char(n)` 按字符数校验；`text` 无长度限制但单行超页报错
- 整数除法向零截断；除零报错
- ORDER BY 可引用 SELECT 别名；JOIN 中同名非限定列报 ambiguous
- 子查询不相关（不引用外层列），视图可叠在 JOIN 中、可套视图
- 显式事务内执行 DDL 报错

## 架构（SQL 的一生）

```
SQL 字符串
  → lexer          分词（int/float/str/标识符/标点/注释）
  → parser         手写递归下降 → AST
  → executor       常量求值/过滤/投影/聚合/分组/排序/limit/连接/索引扫描
  → 规则优化器      AND 链提取索引谓词（EXPLAIN 可观测）
  → 子查询物化      执行前一次性求值 IN/EXISTS/标量子查询并改写为字面量
  → B+ 树索引      保序字节键、分裂/借用/合并、范围扫描
  → MVCC           快照隔离、undo 回滚、BEGIN/COMMIT/ROLLBACK
  → WAL            提交时 fsync；崩溃后重放已提交事务
  → slotted page   8KB、槽目录、变长条目
  → HeapFile       Rid 寻址、多页 first-fit
  → BufferPool     8KB 帧、LRU、脏页写回
  → DiskManager    分页文件 IO
```

文件布局：`catalog.bin`（元数据 + 事务簿记 + 视图定义）、`wal.bin`（预写日志，
干净关闭后清空）、`tables/*.dbf`（每表一个堆文件）、`indexes/*.idxf`
（每索引一棵 B+ 树）。

## 并发与事务

- 快照隔离：事务开始时记录已提交事务快照；每行带 creator/deleter 事务号，
  读时按可见性规则过滤；UPDATE = 删除标记 + 新版本
- 单写者模型：全部执行在全局 Mutex 下串行；多连接各自持有 Session，
  支持跨语句事务，连接断开自动回滚
- 崩溃恢复：WAL 只重放有 COMMIT 记录的事务，重放幂等（精确 Rid 回写 +
  删除标记条件重放）；索引页属派生数据，恢复时对被触及的表重建

## 测试

`cargo test` 跑 239 个测试，覆盖词法/语法/求值/聚合/连接/索引/持久化/事务/
WAL 恢复/存储层/网络协议等。集成测试的 `with_dbs` 模式让同一用例在内存后端
与文件后端各跑一遍；WAL 测试用 `Database::simulate_crash()` 模拟进程被杀。

## 依赖

仅三个：`tokio`（异步运行时与网络）、`thiserror`、`tempfile`（测试）。
