# chibidb 交接文档（Handoff）

> 一份给下一个 Agent / 开发者的完整上下文。读完本文档即可在不了解前序对话的情况下继续开发。
> 最后更新：P10 完成 + P7 全部（双引擎）+ P8.1 外存 LOB；437 tests 全绿、clippy 零警告。

---

## 1. 项目是什么

用纯 Rust + tokio 手写的教学型单机数据库，对标 OceanBase miniob（2024 版）。从零开始按 **TDD 红绿灯** 节奏演化，每个小步先写失败测试（红），再最小实现（绿），提交保持细粒度。

已达成的完整栈：

```
SQL 字符串
  → lexer (token)
  → parser (AST，手写递归下降)
  → executor（常量求值/过滤/投影/聚合/分组/排序/limit/连接/索引扫描）
  → 规则式优化器（AND 链提取索引谓词，EXPLAIN 可观测）
   → B+ 树索引（保序字节键、分裂/借用/合并、范围扫描）
   → MVCC（快照隔离、undo 回滚、BEGIN/COMMIT/ROLLBACK）
   → WAL（提交时 fsync、崩溃后重放已提交事务、干净关闭即 checkpoint）
  → slotted page（8KB、槽目录、删除压实）
  → HeapFile（Rid 寻址、多页 first-fit）
  → BufferPool（8KB 帧、LRU、脏页写回、Drop 落盘）
  → DiskManager（分页文件 IO）
  → 文件布局：catalog.bin + tables/*.dbf + indexes/*.idxf
  → 两种前端：本地 REPL（tokio stdin/stdout）与 TCP server + client（长度前缀二进制协议）
```

---

## 2. 环境与常用命令

- Rust **1.98.1**（stable-msvc，Windows 11 + PowerShell 5.1；跨平台代码，无平台特定 API）
- 依赖仅三个：`tokio`（io-std/io-util/macros/net/rt-multi-thread/sync）、`thiserror`、`tempfile`
- 运行环境注意：**命令行是 Windows PowerShell**，具体陷阱见 §8

```powershell
cargo test                      # 全量回归（296 tests，20+ 个测试二进制）
cargo test --test trx           # 单个测试文件
cargo test --quiet              # 安静模式（注意配合退出码判断，见 §8）
cargo build
cargo run -q                    # 内存数据库 REPL（实际是临时目录，见 §7）
cargo run -q -- <dir>           # 文件数据库 REPL
cargo run -q -- serve <dir>     # TCP server，监听 127.0.0.1:5678
cargo run -q -- client [addr]   # 交互式客户端
```

全链路冒烟（起 server → client 建表/插入/跨语句事务 → 强杀进程 → 复开验证 WAL 恢复）已脚本化：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\smoke.ps1   # 期望输出 SMOKE OK
```

启动时会在**当前工作目录**读取 `config.toml`（缺失即全用默认值）。已生效条目：
`storage.buffer_pool_frames`、`storage.double_write`、`wal.checkpoint_threshold`、`server.addr`、
`server.thread_model`/`worker_threads`。其余条目
（`storage.page_size/default_engine/double_write/inline_lob_limit`、`execution.mode`、
`auth.enabled`、`server.protocols`）是后续阶段的预留位；其中 `page_size` 当前必须等于编译期
`PAGE_SIZE`，否则打开数据库时报错（动态页大小属 P6 存储抽象）。

---

## 3. 代码地图

### 3.1 源码（`src/`）

| 文件 | 职责 | 关键类型/入口 |
|---|---|---|
| `main.rs` | 二进制入口：REPL / `serve` / `client` 子命令 | |
| `lib.rs` | **核心门面**：`Database`、打开/建库、会话执行、存储操作、索引维护、undo 回滚 | `Database::open` / `open_in_memory` / `execute_sql` / `execute_sql_with` |
| `lexer.rs` | 分词：整数/浮点/字符串/标识符/标点/`--` 注释 | `lex(src) -> Result<Vec<Token>>` |
| `parser.rs` | 递归下降解析全部 SQL（文法见 §5） | `parse(sql) -> Result<Vec<Stmt>>` |
| `ast.rs` | AST：`Stmt` / `Expr` / `SelectStmt` / DDL / `TrxCtl` 等 | |
| `exec/mod.rs` | 执行器入口：语句分发、DDL（`SELECT`/DML 必须走算子计划，此处报错）、`coerce` | `execute(db, trx, stmt)` |
| `exec/dml.rs` | DML 命令算子：`InsertOp`/`UpdateOp`/`DeleteOp`（`open` 执行写入、`next` 无行、`output_kind = Command`） | |
| `exec/eval.rs` | 表达式双上下文求值（`EvalCtx` 带父链的 Row/Group 作用域，`resolve_column` 逐层向外）、算子、三值逻辑、LIKE、标量函数、`eval_const` | `eval` / `eval_const` |
| `exec/aggregate.rs` | 聚合求值、GROUP BY/HAVING、ORDER BY、DISTINCT、LIMIT 的共享逻辑 | `grouped_select_rows` / `sort_rows` / `eval_aggregate` |
| `exec/plan.rs` | EXPLAIN 计划、规则式索引访问路径（sargable）、同列上下界合并为范围扫、**有序索引扫描判定**（`order_by_matches`）、`plan_index_scan` | `execute_explain` / `plan_index_scan` |
| `exec/operator.rs` | 火山算子：`PhysicalOperator`（`output_kind`）+ `ExecContext{db,trx,outer}` + `TableScan`/`IndexScan`/`ViewScan`/`ConstantScan`/`Filter`/`Project`/`Distinct`/`Sort`/`GroupBy`/`NestedLoopJoin`/`HashJoin`/`Union`/`Limit`；`build_select`/`build_statement` 递归建树；`Database::collect_plan` 驱动 | |
| `exec/subquery.rs` | 子查询（IN/EXISTS/标量）：按当前外层行构建并运行算子子计划，改写为字面量；`eval_bound`/`eval_predicate_bound` 是接入点 | `bind_expr` |
| `value.rs` | `Value`：Null/Bool/Int(i64)/Float(f64)/Str/Date(i32 纪元天数)/Text | Display 决定 REPL 输出 |
| `datetime.rs` | 日期校验：civil-date 算法（Hinnant），`'YYYY-MM-DD'` 比较时隐式转日期 | `parse_date` |
| `trx.rs` | 事务：`Session`（trx + `current_db`）/ `TrxState`（id+快照+undo）/ 可见性判定 / `Undo` | `TrxState::visible` |
| `result.rs` | `ResultSet::Message / Rows` | |
| `render.rs` | 对齐表格渲染（REPL 与 client 共用） | `write_result` |
| `repl.rs` | 本地 REPL（`db>` 提示符、exit/quit），持 `Instance` | `run_repl` |
| `server.rs` | tokio 只负责 accept；`ThreadHandler` 把连接派发到阻塞线程（`per-connection` 或 `thread-pool`，config `server.thread_model`/`worker_threads`），同步执行 SQL | `serve(SharedInstance, TcpListener)` |
| `client.rs` | TCP 客户端 | `run_client` |
| `wire.rs` | ResultSet/帧二进制编解码 | `encode_result_frame` / `decode_frame` |
| `protocol.rs` | 前端编解码接缝：`Protocol` trait + `TextProtocol`（`[u32 len][sql]` 请求 / 帧响应） | `decode_request` / `encode_success` / `encode_failure` |
| `pipeline.rs` | SQL 阶段：`Stage`/`Pipeline`/`SqlEvent`；`ResolveStage`（表/视图存在性）、`OptimizeStage`（记录 `plan` 文本 + 建单表算子物理计划）、`ExecuteStage`（优先跑算子，否则 `exec::execute`） | `Pipeline::run` |
| `instance.rs` | 单实例多库：数据根 `<db>/` + 系统库 `chibi_meta/`；每库一个 `parking_lot::Mutex<Database>`（**跨库并行、库内串行**），系统库同样受锁保护；`execute_with` 顶层入口（拦截库/用户/权限语句，其余路由到 current_db） | `Instance::open` / `execute_with` / `with_database_mut` |
| `config.rs` | 全局配置中心：`Config`（storage/wal/server/execution/auth），`config.toml` 加载、默认值、校验 | `Config::load` / `from_toml_str` / `validate` |
| `catalog/mod.rs` | `Catalog`：`Table`/`HeapStore`/`IndexEntry`、`Schema`/`ColumnDesc`（带 `owner`）、`resolve()` 歧义检测 | |
| `catalog/meta.rs` | catalog.bin 自描述格式，魔数 **CHIDCAT6** + 统一文件头（事务簿记 + 视图定义 + 列约束 + 唯一索引标记） | `CatalogSnapshot` |
| `storage/page.rs` | 页常量：`PAGE_SIZE=8192`、`FileId=u32`、`PageNo=u32`、`zeroed_page` | |
| `storage/disk.rs` | `DiskManager`：分页文件读写、建文件、可选 Double-Write Buffer 的 stage/sync/reset | |
| `storage/header.rs` | 统一文件头 `[magic8][version u16][kind u8][page_size u32]`；`write_header`/`read_header` 校验版本/类型/页大小 | `FORMAT_VERSION` |
| `storage/dwb.rs` | Double-Write Buffer：flush 前 stage + sync，崩溃后 `recover` 按路径回写再截断 | `DoubleWrite` / `recover` |
| `storage/buffer.rs` | `BufferPool`：帧数来自 config、LRU `VecDeque`、脏页写回（`flush_all` 走 DWB）、Drop flush；`with_page(file,no,f)` 闭包式访问（访问即脏）、`read_page`（只读不脏） | |
| `storage/slotted.rs` | slotted 页纯函数：槽目录、变长条目、`page_insert`（空槽复用+压实）、`page_get/iter/delete/write` | |
| `storage/engine.rs` | 存储读接缝：`TableEngine`/`RowScanner` trait + `HeapEngine`（流式逐页扫描） | `HeapEngine::scan` |
| `storage/heap.rs` | `HeapFile`：page 0 文件头（魔数 **CHIDHEAP** + 统一头；行带 MVCC 字段）、first-fit 多页、`insert/get/delete(物理)/delete_mark(MVCC)/for_each` | `Rid{page_no,slot}` |
| `storage/codec.rs` | 行/记录编码：自描述 tag（Null=00/Int=01/Float=02/Str=03/Bool=04/Date=05/Text）、值计数前缀；`encode_row/decode_row`；**版本化记录** `encode_record(creator,deleter,row)`（前 8 字节两个隐藏 u32） | |
| `index/key.rs` | 索引保序键编码：int 符号翻转大端、float 保序变换、str+NUL 结尾、date 符号翻转、null=0x00 | `encode_key` |
| `index/node.rs` | B+ 树节点页：叶/内部条目、lower/upper bound、bytes 占用阈值 25%、`page_write` 原位重写 | |
| `index/btree.rs` | B+ 树主体：`init/open/open_or_repair/at`、递归插入双级分裂长高、search（跨叶重复键回退）、scan_range 叶链、delete 借用/合并/根收缩（~880 行） | |
| `wal.rs` | 预写日志：帧 `[u32 len][u8 type][u32 trx][payload]`，Record::Insert/DeleteMark/Commit，追加 + `sync()`（提交点）+ `truncate()`（checkpoint）；`plan_recovery` 解析日志（容忍截断尾帧），纯函数有单测 | `Wal` / `plan_recovery` |

### 3.2 测试（`tests/`，27 个文件 / 296 tests）

- 与源码分层对应：`lexer / parser / eval / agg / join / db / db_index / db_persist / trx / wal / storage_* / index_* / wire / server / repl / datetime / codec / catalog_meta / miniob_compat`
- `miniob_compat`：student/course/sc 端到端组合场景（CRUD+聚合、分组/having、内外连接、不相关子查询、索引/EXPLAIN）
- `correlated`：相关 EXISTS/NOT EXISTS/标量（投影与 WHERE）、多外层列、跨两级引用外层列
- `constraints`：NOT NULL/DEFAULT/列清单 INSERT、PK/UNIQUE 唯一性（含 UPDATE、多行批内、NULL 语义）、约束索引不可 DROP、重启后约束仍在
- `union`：UNION ALL 拼接、UNION 去重、列名取左侧、ORDER BY/LIMIT 管全体、链式混用、列数校验
- `bench`：`#[ignore]` 的索引 vs 全表扫计时，默认不进回归；`cargo test --release --test bench -- --ignored --nocapture`
- `tests/wal.rs` 用 `Database::simulate_crash()`（跳过 BufferPool Drop flush）模拟 SIGKILL，配合自己的 `tempfile::TempDir` 复开同一目录
- 集成测试常用模式：
  - `with_dbs(|db| {...})`：同一用例在临时目录后端和内存（临时目录）后端各跑一遍
  - `Database::open_in_memory().unwrap()` 返回 **Result**（注意是 Result，旧代码曾是直接值）
  - 事务测试用 `Session::new()` + `execute_sql_with`
  - 断言习惯：比较整个 `Vec<Vec<Value>>`（`Value` 实现了 PartialEq/Debug）

---

## 4. 里程碑进度

| 里程碑 | 状态 | 收尾提交 |
|---|---|---|
| M0 REPL 骨架 | ✅ | `fafa547` |
| M1 词法分析（int/float/str/id/注释/标点） | ✅ | `7600546` |
| M2 AST 与解析（表达式/比较/三值逻辑/DDL/DML/select/delete/update/index/explain/聚合/group/order/limit/join） | ✅ | `f440173` |
| M3 执行器骨架 | ✅ | `da88ec0` |
| M4 DML（CRUD + IS NULL） | ✅ | `4a8ee72` |
| M5 BufferPool（DiskManager/BufferPool/SlottedPage） | ✅ | `0bec54c` |
| M6 HeapFile + 行 codec + 双后端统一 + catalog 持久化（重启不丢数据） | ✅ | `75bcae5` |
| M7 catalog 恢复 + `chibidb <dir>` | ✅ | （含在 M6 系列） |
| M8 类型系统：NULL 三值逻辑、DATE（严格校验）、TEXT（页界） | ✅ | `b9d0175` |
| M9 B+ 树索引全套 + CREATE/DROP INDEX + DML 维护 + 规则优化器 + EXPLAIN | ✅ | `7d20698` |
| M10 查询能力：COUNT/SUM/AVG/MIN/MAX、GROUP BY、HAVING、ORDER BY（多键+NULL 排序）、LIMIT/OFFSET、嵌套循环 INNER JOIN（逗号 + JOIN..ON、别名、限定列） | ✅ | `f440173` |
| M11 子查询/视图 | ✅ `IN (值列表)`（parser 脱糖，三值语义）`7c662bd`；不相关子查询 IN/EXISTS/标量（执行前物化改写）`3a848ca`；CREATE/DROP VIEW + FROM 展开 + 持久化 `41429e9` | `41429e9` |
| M12 事务（MVCC + WAL） | ✅ M12.1 MVCC 核心 `568f62d`；M12.3 WAL+崩溃恢复 `0fe605d`（M12.2 update/delete MVCC 化已并入 M12.1） | `0fe605d` |
| M13 TCP server + client + wire 协议 | ✅ | `d06103c` |
| M14 加固 | ✅ DROP TABLE（`d0e239c`）、跨语句事务会话修复（`bcfd6ee`）、clippy 清零（`0023a43`）、README + 冒烟脚本（`46c8637`） | `46c8637` |
| M15 查询/运维增强 | ✅ CHECKPOINT 语句 + WAL 预算护栏（`c8e060e`）、DISTINCT（`2389f99`）、LEFT [OUTER] JOIN（`5f31ea2`） | `5f31ea2` |
| M16 空间回收 | ✅ VACUUM：物理回收已提交删除标记行/孤儿版本 + stale 索引项清理；flush() 开事务守卫（`975dab2`） | `975dab2` |
| M17 表达式面 | ✅ `%` 标点入 lexer；`expr [NOT] LIKE`（`%`/`_` 通配，无转义）；MOD 运算符；字符串函数 concat/upper/lower/length/substring；exec.rs 拆分为 exec/ 六模块 | `d3a85ad` |
| M18 查询/性能 | ✅ miniob 经典 student/course/sc 端到端回归；忽略式索引基准（`tests/bench.rs`）；修复单表索引扫描仍先全表扫的空转；AND 链同列上下界合并为一段范围扫；只读事务不再重写 catalog | `5c4e26e` + `546b601` |
| M19 相关子查询 | ✅ `EvalCtx` 改为带父链的作用域（列解析逐层向外）；子查询改为在求值点按当前行/组物化（`bind_expr`/`eval_bound`），支持多层嵌套的相关引用 | `8dce9a3` |
| M20 表约束 | ✅ 列选项 PRIMARY KEY / UNIQUE / NOT NULL / DEFAULT 解析并持久化（CHIDCAT5）；INSERT 列清单 + DEFAULT 补全；NOT NULL 在 INSERT/UPDATE 校验；PK/UNIQUE 自动建唯一索引并在 DML 查重（`duplicate key`），约束索引不可单独 DROP | `18e940d`…`e381c87` |
| M21 RIGHT JOIN | ✅ `JoinKind::Right`，与 LEFT 对称：未匹配右行保留、左列补 NULL（补宽 = 加入第 i 表前的累计列数） | `3433843` |
| M22 UNION | ✅ `SelectStmt.set_ops: Vec<(bool /*all*/, Box<SelectStmt>)>` 左结合；列数必须一致；UNION 去重、UNION ALL 不去重；尾部 ORDER BY/LIMIT 作用于整个并集（ORDER BY 解析输出列名） | `83b2a23` |
| M23 SQL 小补齐 | ✅ 聚合 `DISTINCT`（count/sum/avg/min/max）；`LIKE ... ESCAPE`；UPDATE/DELETE 的 WHERE 与 UPDATE SET 支持子查询 | `5f8be7f`…`8d2adcd` |
| M24 元数据锁 | ✅ 其它会话有开事务时，CREATE/DROP TABLE/INDEX/VIEW 报 `schema is locked by an open transaction`（`execute` 入口统一守卫） | `2a3a5a0` |
| M25 消除排序 | ✅ 单表 + 索引扫描 + `ORDER BY` 单列且为该索引列升序时跳过 `sort_rows`（叶链本身有序）；EXPLAIN 报 `OrderedIndexScan`；DESC/异列仍排序 | `e897b08` |

---

## 5. SQL 方言现状

```sql
-- DDL
CREATE TABLE t (id int primary key, name char(10) not null,
                score float default 0, email char(20) unique);
CREATE INDEX idx_name ON t (col);
DROP INDEX idx_name;                       -- 约束索引（PK/UNIQUE）拒绝 DROP
DROP TABLE t;
CREATE VIEW v AS SELECT ...;               -- 定义以原 SQL 文本存入 catalog
DROP VIEW v;
-- DML
INSERT INTO t VALUES (1,'a',1.5),(2,'b',2.0);   -- 多值行，字面量允许负号，null 关键字
INSERT INTO t (name, id) VALUES ('a', 1);        -- 列清单（可乱序/省略，省略列取 DEFAULT）
UPDATE t SET score = (SELECT max(score) FROM other) WHERE id < 10;  -- SET/WHERE 支持子查询
DELETE FROM t WHERE id IN (SELECT id FROM other);
-- 查询
SELECT [DISTINCT] * | expr [AS alias] (, ...)
  [UNION [ALL] SELECT ...]*                 -- 并集（左结合；尾部 ORDER BY/LIMIT 管全体）
  FROM tref (, tref)*                      -- 逗号 = cross join
  [JOIN | LEFT [OUTER] | RIGHT [OUTER] JOIN tref ON cond]* -- INNER/LEFT/RIGHT（未匹配侧补 NULL）
  [WHERE expr]
  [GROUP BY expr (, expr)*]
  [HAVING expr]
  [ORDER BY expr [ASC|DESC] (, ...)*]      -- NULL 升序在前降序在后，稳定排序
  [LIMIT n [OFFSET m]]
-- 聚合：count(*)/count(x) 忽略 null / sum / avg（恒 float）/ min / max；空集 sum/avg/min/max → NULL
--       支持 DISTINCT：count/sum/avg/min/max distinct expr（count(distinct *) 非法）
-- 表达式：+ - * / %（int 截断/取模、checked overflow）、and/or/not（SQL 三值逻辑）、
--         = <> < <= > >=、is [not] null、括号、
--         expr [NOT] LIKE 'pattern' [ESCAPE 'c']（`%` 任意串、`_` 单字符，区分大小写）、
--         字符串函数 concat / upper / lower / length / substring(s, start[, len])、
--         expr [NOT] IN (值列表)、expr [NOT] IN (SELECT..)（单列，可相关）、
--         [NOT] EXISTS (SELECT..)、标量 (SELECT..)；子查询可引用外层列（多层相关）
-- 事务
BEGIN; COMMIT; ROLLBACK;
CHECKPOINT;                                -- flush_all + save_catalog + 截断 wal；有开事务时报错
VACUUM;                                    -- 物理回收（见 §6.4）；有开事务时报错
EXPLAIN SELECT ...;                        -- 输出 FullScan / IndexScan / NestedLoopJoin
```

子查询/视图的实现要点（改相关代码前必读）：

- `IN (值列表)` 在 **parser 里脱糖**成 OR/AND 比较链，三值语义免费正确（列表含 NULL 时 NOT IN 永不 TRUE）
- 子查询（InSubquery/Exists/ScalarSubquery）走 `exec/subquery.rs`：在每个求值点用 `eval_bound`/`eval_predicate_bound` 先 `bind_expr`，把子查询**按当前行/组上下文执行并改写为 `Expr::Value` 字面量**，之后 eval 仍是无上下文纯函数。**相关子查询**通过 `EvalCtx` 父链实现：内层 schema 解析不到列时逐层向外查（`resolve_column`），最内层歧义不再外查；`execute_select` 多了 `outer: Option<&EvalCtx>` 参数。嵌套/多层相关自然递归
- 相关子查询为**逐行物化、无缓存**：不相关子查询也会按行重复执行（正确但非最优，见 §8）
- 标量子查询：0 行 → NULL，>1 行或 >1 列报错；IN 子查询要求单列
- 视图：定义 SQL 原文存 catalog（parser 用 token 偏移切片，需 `Parser.src`）；查询时 `exec::from_source` 在 FROM 处展开（真实表走 MVCC，视图递归执行其 select）；视图列 dtype 用 Text 占位（查询路径不用 dtype）；视图无索引（find_sargable 直接跳过）；CREATE VIEW 时试执行一次做校验（表/列存在性）
- catalog 格式已升 **CHIDCAT4**（views 字段）；视图名与表名互斥占用

细节语义（已被测试锁定，不要随意改）：

- 标识符大小写不敏感（表名/列名/关键字），表名与列名大小写敏感匹配保持现状
- 整数除法向零截断；除零报错；`1/0=2` 三值逻辑：NULL 参与比较 → NULL（UNKNOWN），WHERE 只放行 TRUE
- `date` 列插入字符串时严格按 `YYYY-MM-DD` 校验（闰年用 Hinnant 算法）；与字符串比较时隐式转换
- `char(n)` 按字符数校验；`text` 无长度限制但单行记录超 8KB 报错
- `LIKE`：`%` 匹配任意长（含空）子串、`_` 匹配单字符，逐字符比较（非字节），区分大小写，无转义；任一侧 NULL → NULL
- 字符串函数：`concat` 把任意标量转文本拼接、任一参数 NULL → NULL；`upper`/`lower`/`length`/`substring` 仅接受 Str（否则 type mismatch），NULL 传播；`length` 按字符数；`substring` 下标 1 起，越界得空串，start<1/len<0 报错；`substr` 为别名；未知函数运行时报错
- `%` 为取模：int 用 checked_rem、float 用 fmod，模零报错
- 聚合无 GROUP BY 时裸列出现在 SELECT 会报错；GROUP BY 下其它列取首行（宽松语义）
- ORDER BY 可引用 SELECT 别名（如 `count(*) as total ... order by total`）
- JOIN 中同名非限定列报 `ambiguous column`，用 `alias.col` 限定
- LIMIT 只接受非负整数字面量
- 显式事务内执行 DDL 报错（`DDL inside a transaction is not supported`）
- 列约束：`primary key` 隐含 `not null` + `unique`；PK/UNIQUE 各自动建名为 `__unique_<table>_<column>` 的唯一索引（约束索引不可单独 DROP）；UNIQUE 允许多个 NULL，PK 因 NOT NULL 不允许；INSERT/UPDATE 走索引查重并过 MVCC 可见性，报 `duplicate key`；同语句内多行用 claimed-key 集合查重；`INSERT INTO t (cols) VALUES` 省略列取 DEFAULT，否则 NULL；DEFAULT 在建表时按列类型 coerce 成 `Value` 存 catalog

---

## 6. 存储与 MVCC 设计（继续动这里前必读）

### 6.1 文件布局

```
<data_dir>/
  catalog.bin            # 魔数 CHIDCAT6 + 统一文件头 + next_table_file/next_index_file/next_trx_id/committed[]
                          # + 表元数据（列定义+file_no）+ 索引元数据（name/table/column/file_no）
  wal.bin                # 预写日志（见 §6.3）；干净关闭/flush 后为 0 字节
  tables/000000.dbf ...  # 每表一个 HeapFile；page 0 头魔数 CHIDHEAP + 统一头，数据页从 1 起，first-fit
  indexes/000000.idxf ...# 每索引一个 B+ 树文件；page 0 头魔数 CHIDBITX + 统一头
  dwb.bin                # Double-Write Buffer（config.storage.double_write 开启时）
```

### 6.2 MVCC 现状（M12.1）

- 每条堆记录物理格式：`[u32 creator_trx][u32 deleter_trx][行 codec]`（见 `codec::encode_record/decode_record`）
- 事务状态在 `trx.rs`：
  - `Session { trx: Option<TrxState> }`（server 每连接一个；REPL 一个）
  - `TrxState { id, snapshot: HashSet<u32> 已提交集合快照, undo, explicit }`
  - 可见性：creator ∈ {0, self, snapshot} 且 deleter=0 或 deleter=self 的反向规则——
    `deleted_for_me = deleter != 0 && (deleter == self.id || snapshot.contains(deleter))`
    **注意：自己删的行对自己立即可见性为假（行消失）**，这是曾经写反过的坑
- 自动提交：`execute_sql`（无 session）每条语句一个临时事务；语句失败 → 整条语句 undo
- DML 语义：
  - INSERT → 新版本（creator=trx）；undo 物理删除该 rid 并删对应索引项
  - DELETE → `HeapFile::delete_mark`（原位改写 8 字节，记录长度不变；**不做物理回收**）；undo 清标记
  - UPDATE → 标记旧版本 + 追加新版本（新 Rid）；索引只追加新键项，旧项靠读时可见性过滤；undo 删新版本+索引项+解除旧标记
- 提交：`committed_trxs.insert(id)` 后，**只有写事务**（undo 非空）才追加 Commit 帧 + `wal.sync()` + `save_catalog()`；只读事务只更新内存 committed 集就返回（无版本行引用其 id，落盘无意义，且避免每条 SELECT 重写 catalog）
- 读路径统一走 `decode_visible(records, trx)`；JOIN 流水线、索引扫描（store_get_records）都要过这层
- 并发模型：全局单 Mutex 串行化，允许多个 BEGIN 并存但执行串行；连接断开时应 `rollback_session`（server.rs 当前在连接结束路径，确认已接入）
- 索引与可见性：索引本身**不含** trx 信息，扫到 rid 后回表 + 可见性过滤；UPDATE/DELETE 会积累 stale 索引项（空间债）

### 6.3 WAL + 崩溃恢复（M12.3，已实现）

帧格式 `[u32 len][u8 type][u32 trx_id][payload]`（len 覆盖 len 之后的所有字节；type 1=Insert、2=DeleteMark、3=Commit；解析遇截断尾帧/未知 type 即停止）。Update 记为 DeleteMark+Insert 两条。

- **写时机**：`store_insert` / `store_delete_mark` / `store_update_versions`（lib.rs）在堆操作成功后追加；`commit_trx(trx_id, wrote)` 在 `committed_trxs.insert` 后追加 Commit 帧 + `sync_all()`（真正的提交点），随后 save_catalog。**只读事务（undo 空）不记 Commit、不 fsync**
- **恢复**（`Database::open` 末尾，`recover_from_wal`）：
  - 只重放 wal 中有 Commit 帧的事务，按 Commit 帧出现顺序（=提交顺序）；重放幂等：Insert 仅当目标槽空才用 `slotted::page_put_at` 原位写回原 Rid，DeleteMark 仅当记录存在且 deleter==0 才打标
  - 未提交事务的脏页若曾被 LRU 驱逐落盘，其行天然不可见（creator 不在 committed 集），无需 UNDO
  - `next_trx_id` 提升到 max(wal 中见过的 trx id)+1（含未提交的），防止 id 复用把幽灵行"过继"给新事务
  - wal 的 Commit id 与 catalog committed 集取并集，若 catalog 缺失则修复并 save_catalog（wal 是提交事实来源）
  - **索引重建**：被重放触及的表（含页0被修复的索引文件所属表）全部重建索引——B+ 树页与堆页一样可能没落盘，且 B+ 树 insert 不幂等（重放会产生重复键）。重建前先 `pool.discard_file`（丢缓存）+ `truncate_file` + `BTree::init`，再全堆扫描重灌
- **checkpoint**：`flush()` = flush_all → save_catalog → wal.truncate()。干净关闭（REPL 退出）即 0 字节日志。M15 起另有两条路：`CHECKPOINT` 语句（`execute_checkpoint`：自身 autocommit 事务不计入、显式事务或其他会话开事务时拒绝）与**自动触发**——`commit_trx` 末尾若 `wal.len() > wal_checkpoint_threshold`（默认 8MB，`set_wal_checkpoint_threshold` 可改）且 `open_trxs` 为空则调 flush()
- **open_trxs 注册表**：Database 持有已 begin 未终局的事务 id 集；begin 两处 insert，commit_trx/Rollback 臂/autocommit 错误路径/rollback_session remove。**它保护日志截断**：截断时若其他事务未提交，其 Commit 帧之后到达会丢 redo → 数据丢失，故护栏必须存在
- **页0修复**：`HeapFile::open_or_repair` / `BTree::open_or_repair`——catalog 已记录但页0魔数没落盘（CREATE TABLE/INDEX 后立刻崩溃）时原位重写魔数；索引文件被修复会触发上述重建
- **测试**：`tests/wal.rs` 8 个集成用例（提交插入/删除/更新/带索引崩溃恢复、未提交不复活+id 不复用、截断尾帧容忍、干净 flush 截断日志、rollback 无残留）；`src/wal.rs` 内 5 个纯函数单测
- 已知边界：全量 checkpoint（无模糊检查点）；日志未压缩

### 6.4 VACUUM 空间回收（M16，已实现）

`Database::vacuum()`（`VACUUM` 语句触发，守卫同 CHECKPOINT：排除自身临时事务 + 拒绝显式/他方开事务）。判定条件（前提 `open_trxs` 为空）：

- 行死亡 = `creator ∉ committed_trxs`（崩溃事务遗留的孤儿版本）**或** `deleter ∈ committed_trxs`（已提交删除标记）
- deleter ∈ open 不可能出现（前提），creator ∈ open 同理；未提交 deleter 的行保留（删除永远不会发生 = 行仍活着）
- 回收动作：先删该行在所有索引上的 (key, rid) 项（**必须**，否则索引扫描回表报 "no record at rid"），再 `HeapFile::delete` 物理删除（page_delete + 压实；槽号稳定，Rid 不失效）
- **无需 WAL 记录**：vacuum 只删除"恢复重放也不会复活"的数据——崩溃后重放按序重演 Insert→DeleteMark，最终可见状态一致（tests/vacuum.rs 的 crash-safe 用例锁定此性质）
- 边界：空页不归还文件（空间留给 first-fit 复用）；`flush()` 新增开事务守卫（截断日志会丢开事务的 redo），CHECKPOINT 走 `flush_inner` 绕过（已自行验证排除自身）

---

## 7. 关键约定与工作流（新 Agent 必须遵守）

1. **TDD 节奏**：一个能力点 = 一个/几个红测试 → 最小实现 → `cargo test` 全绿 → 一个细粒度提交。提交消息用 Conventional Commits：`feat: ...` / `fix: ...` / `refactor: ...` / `chore: ...`
2. **提交前必须看到全量测试通过**。不要把编译错误/红灯混进提交（项目历史里有过两次，随后紧跟 fix 提交，尽量避免）
3. 测试与实现都放在 `tests/` 与 `src/` 既有分层中；纯函数层（lexer/node/slotted/codec/key）直接单测，系统行为写集成测试（`tests/db*.rs` 风格）
4. 保持向后兼容的取舍：旧数据文件格式变更要 bump 魔数（heap 已 CHD2，catalog 已 CHIDCAT4，btree CHIDBTX1）
5. `open_in_memory()` 实际是 `tempfile::TempDir` 后端（M9 时统一的，避免双后端分叉），Database 持有 `_temp: Option<TempDir>` 自动清理
6. 大的重构优先于打补丁：如 M9 把 Mem/Heap 双存储统一成 Heap-only（`f13f83c`），可参考其风格
7. 不要引入新依赖，除非确有必要（目前仅 3 个直接依赖）

### PowerShell 5.1 踩坑记录（血泪）

- **不要用 `Set-Content` 写 Rust 源文件**：默认编码非 UTF-8，会把中文注释写成非法字节导致 rustc 报 "stream did not contain valid UTF-8"。必须写文件时用 `[System.IO.File]::WriteAllText($path, $content, [System.Text.UTF8Encoding]::new($false))`，或直接用编辑工具
- **不要靠 `if ($?)` / `$LASTEXITCODE` 在管道后做 cargo 成败判断**：管道后二者都不可靠，曾导致红灯被提交或提交悄悄没执行。正确做法：分两条命令，先看测试输出全文，再显式 `git add; git commit`
- **Windows 上 append 模式打开的文件句柄不能 `set_len`**（os error 5 拒绝访问）：wal 的 checkpoint 截断因此用普通 write 句柄 + 每次追加前 `seek(End)`，不要改回 `.append(true)`
- `Get-Content -Raw` 的正则替换多行内容时注意 `\r?\n`，替换后用 `Set-Content -NoNewline`
- cargo test 全量在本机约 30~60 秒，含 server 测试（tokio 网络），给足超时（180000ms+）
- 管道编码问题导致中文乱码时，先怀疑文件编码再怀疑逻辑

### 已修过的典型 bug（改相关代码时警惕回归）

- 日期比较在 `cmp_values` 重构时方向被写反过（Date 在左/在右）
- NULL 比较语义：`cmp_values` 返回 `Option<Ordering>`（None=NULL），但 NULL 必须先短路成 `Value::Null`，不能和 NaN 的 None 混用
- ORDER BY 比较器必须对 NULL 给出**一致全序**（null,null=Equal；null,x=Less；x,null=Greater），否则 sort 结果未定义
- B+ 树：重复键跨叶分裂后点查要沿叶链回退；UPDATE 后必须用**新 Rid** 写索引，且即使索引列未变也要删旧插新（rid 已失效）
- B+ 树 `locate_child` 曾误读父节点类型当子节点类型（读 child 自己的 type）
- JOIN 的 ON 条件索引错位：`on[i]` 应在处理第 `i+1` 张表后求值（即 `on.get(i-1)`）
- `select *` 展开到 JOIN schema 时必须用 QualifiedColumn（owner 限定），否则歧义
- 客户端读到 Error 帧后必须把尾随 Done 帧消费掉，否则污染下一个请求

---

## 8. 已知技术债 / 明确的边界

- 无 FULL OUTER JOIN、无 ALTER TABLE、无视图上的 INSERT/UPDATE（视图只读）
- 相关子查询**无缓存**：每个外层行都会重新执行子查询（不相关子查询也因此按行重复执行）
- LIKE 无 ESCAPE 转义；无其它字符串/数学函数（仅 concat/upper/lower/length/substring 与 `%` 取模）
- 索引访问路径只做单列：AND 链里若同列出现多个下界（或上界）只保留最后一个，不做“取更紧者”的择优化；跨列不合并
- 排序消除仅覆盖「单表 + 索引扫描 + ORDER BY 单列 = 该索引列 + ASC」；DESC 未做反向叶链迭代，多列/异列仍走 `sort_rows`
- DISTINCT + ORDER BY 引用非投影列时，保留哪一行是按扫描顺序首个（标准 SQL 视为非法，未做校验）
- VACUUM 回收的空页不归还文件系统（页留给 first-fit 复用）；空页只在该表变小时浪费
- 单 Mutex 单 writer 串行化；无死锁检测；vacuum 之外长事务 + 未提交孤儿版本仍会占空间
- BufferPool 无预读；WAL checkpoint 是全量截断（有预算护栏但无模糊检查点）；first-fit 插入是 O(页数)，建 5 万行表的主要耗时即在此（基准测试因此偏慢）
- DDL 隔离：其它会话持有开事务时任何 CREATE/DROP 都被拒绝（M24，`schema is locked by an open transaction`）；这是粗粒度全库锁，无按表锁
- `exec/` 已按职责拆分（mod/eval/aggregate/join/plan/subquery，见 §3.1）；跨模块共享项用 `pub(crate)`，`eval_const` 经 `exec::eval_const` 重导出
- 索引访问路径已修：单表 SELECT 命中索引时不再先全表扫；`id >= a and id < b` 合并成一段范围扫。基准（release/5 万行）：点查 ~19µs vs ~27ms，单边范围 ~54µs vs ~24ms，双边范围 ~0.11ms vs ~29ms

---

## 9. 执行计划（已评审确认，按序执行；上一轮 1–6 全部完成，里程碑见表）

1. **表约束：PRIMARY KEY / UNIQUE / NOT NULL / DEFAULT**（中大，miniob 对齐最大缺口，M20）✅
   - `CREATE TABLE` 字段选项入 AST/`ColumnDef`；catalog 元数据扩展 → **bump CHIDCAT5**
   - 主键/唯一约束自动建唯一索引（沿用现有 `IndexStore`）；INSERT/UPDATE 前借该索引查重 → `duplicate key` 报错
   - NOT NULL 在 `coerce` 处校验；DEFAULT 值存 catalog，INSERT 缺列时补（需支持 `INSERT INTO t (a,b) VALUES ...` 列清单，当前要求全列）
   - 红测试先行：约束 DDL 解析、违反各约束的报错、重启后约束仍在
2. **RIGHT JOIN**（小，M21）✅：`JoinKind::Right` 与 LEFT 对称；未匹配右行保留、左列补 NULL（补宽 = 加入第 i 表前 `schema.columns.len()`）；parser 支持 `RIGHT [OUTER] JOIN`
3. **UNION / UNION ALL**（中，M22）✅：AST 用 `set_ops` 左结合列表（比计划的单 `combine` 更贴合 SQL 左结合语义）；列数必须一致；UNION 走 `dedup_rows`；尾部 ORDER BY/LIMIT 作用于整个并集，ORDER BY 按输出列名解析（`sort_projected`）
4. **SQL 小补齐**（小，M23）✅：`count(DISTINCT expr)`（推广到 sum/avg/min/max）；LIKE `ESCAPE`（模式先编译成 token）；UPDATE/DELETE 的 WHERE 与 UPDATE SET 支持子查询
5. **元数据锁 / DDL 隔离**（中，M24）✅：`execute` 入口对 DDL 统一守卫，`has_open_trxs_excluding(自身)` 为真时报 `schema is locked by an open transaction`（粗粒度全库锁）
6. **索引有序性消除排序**（中小，M25）✅：`plan::order_by_matches` 判定单表单列 ASC 且等于选路索引列时跳过 `sort_rows`；EXPLAIN 报 `OrderedIndexScan`；`tests/bench.rs` 增 `ORDER BY id` 用例（~0.23ms vs 全表 ~24ms）；DESC 与异列仍排序

工作纪律：TDD 红绿节奏、每项一个里程碑提交、提交前全量 `cargo test` + clippy 清零 + 更新本文档与 README。

进度：**M20–M25 计划全部完成并通过验收**（297 tests，clippy 零警告，改动已按里程碑细粒度提交）。

验收记录（M20–M25 评审）：296 tests 全绿 + clippy 零警告；人工边界复验（跨列同值、DROP TABLE 清约束索引、链式 RIGHT JOIN、ESCAPE 角例、OrderedIndexScan 含 DESC、DML 子查询、恢复路径 `rebuild_indexes` 覆盖约束索引）均通过。**发现并修复 1 处阻断性缺陷**：`check_unique` 的 `claimed` 查重表跨唯一索引共享，同一行两个不同约束列取同值（如 PK 列与 UNIQUE 列同为 1）会被误判 `duplicate key`——红测试复现后按列下标区分修复，见 `abd0dbb`。已知边界（如实记档）：UNION 各臂不做类型统一，混型结果集上比较会报 type mismatch。

提交基线：`0d48d50 feat: add an external LOB store with a chunk-streaming reader`（HEAD）。

---

## 10. 重构路线图与进度（2026 起）

目标：并发从全局单锁迁到线程池/per-transaction；LOB；Stage 流水线（保留手写 parser，新增
Resolve/Optimize 阶段）；执行模型算子化（火山为核、Chunk 可选）；存储多引擎（Heap + LSM）+
Double-Write Buffer；多前端（MySQL/HTTP/Text TCP）；单实例多库 + 系统元数据库 + 用户表；
全局配置中心。恢复/事务最终方案后议，先冻结接口。

已确认决策：sync trait + `spawn_blocking`（非 async trait）；并发随子系统逐步线程安全、
最后收口移除全局锁；旧数据文件不兼容（可丢弃）；依赖放宽（toml+serde/parking_lot/crossbeam 等）；
MySQL 认证先做 `mysql_native_password`、暂不做 TLS。

并发/事务设计（P10.0 定稿）：保持**快照隔离**；线程模型抽象 `ThreadHandler`，提供
`per-connection` 与 `thread-pool` 两种后端（config `server.thread_model` / `worker_threads`）；
分期 P10.1 per-DB `Mutex` 去全局锁 → P10.2 库内 `RwLock` + Catalog `RwLock` 快照 +
BufferPool 页闩 + WAL 组提交 → P10.3 库内多写者，冲突策略**可配置**
（`transaction.conflict = "fcw" | "2pl"`，先 FCW 后 2PL + 死锁检测）。锁库 `parking_lot`
（实现采 `RwLock<Catalog>`，未引入 `arc-swap`：读路径 `clone` committed 快照，后续可换 arc-swap 免拷贝）。
DML 先算子化（P5.5），使执行层统一走算子。

物化执行基准（release，5 万行，`cargo test --release --test bench -- --ignored --nocapture`）：
算子路径下 scan+project 19.0ms、filter tag=3 13.7ms、count(*) 9.9ms、group by tag 20.8ms
（P5 前物化基线分别为 31.1/23.8/18.9/27.2ms）；等值 `HashJoin` 50k×50k + count(*) 93.7ms。

| 阶段 | 内容 | 状态 |
|---|---|---|
| P0 | 基线：297 tests + clippy + bench 记录 | ✅ |
| P1 | 配置中心（`config.rs` + `config.toml` + 接入 DB/server） | ✅ |
| P1.3 | 文件头 `format_version/page_size/engine` 元数据 | ⏩ 延到 P6（`PAGE_SIZE` 现为编译期常量，动态化随存储抽象做） |
| P2 | 抽象接缝：存储读接口 `RowScanner`/`TableEngine` + `HeapEngine`；`Protocol`/`TextProtocol` 编解码；`Stage`/`Pipeline` + `ExecuteStage` | 🟡 三个接缝落地（`d220fcb`、`7b351d6`、`9c5199a`） |
| P2+ | 其余接缝：`TransactionManager`、`PhysicalOperator` | ⬜ |
| P3 | 单实例多库 + 系统元数据库 + 用户/权限 | 🟡 多库 + 前端接入 + 系统库 `chibi_meta`（`databases`/`users`/`privileges`）+ `CREATE/DROP USER` + `GRANT/REVOKE`（`e99c5ef`…`7d8a85f`）；认证/权限尚未在协议层强制、系统表未以 `information_schema` 暴露 |
| P4 | Stage 流水线（Parse/Resolve/Optimize/Execute/Result） | 🟡 `ResolveStage`/`OptimizeStage` 落地（`dc312de`）；Parse 仍在 pipeline 外、Execute 尚未消费 `plan`、Result 写出仍在前端 |
| P5 | 执行模型：基准 → 火山算子 → Chunk | ✅ SELECT 全走算子（单/多表、`HashJoin`、分组聚合、DISTINCT、ORDER、LIMIT、UNION、视图、相关/不相关子查询）；物化 SELECT 主干已删除；Chunk 按评估暂缓 |
| P5.5 | DML 算子化：`Insert`/`Update`/`Delete` 命令算子 | ✅ `InsertOp`/`UpdateOp`/`DeleteOp`（`e620154`） |
| P6 | 存储引擎抽象落地：Heap + Double-Write Buffer | 🟡 统一文件头（`63ede97`）+ 可配置 DWB（`3daa90b`）；`StorageEngine` trait 待 LSM 阶段再扩（`TableEngine` 读接缝已在 P2 落地） |
| P7 | LSM 引擎（Heap + LSM 双引擎，配置选择） | ✅ P7.1：存储引擎接缝完成：`TableStorage`（`TableEngine` + `insert/delete/delete_mark/file_id`，MVCC 语义）与 `HeapEngine` 实现；`Table` 持 `Arc<dyn TableStorage>`，`Database` 的 `store_*`/回滚/vacuum/唯一性检查、`TableScan`/`IndexScan` 均经该接缝（`da4299f`）。P7.2 完成：`src/storage/lsm/memtable.rs` 有序写缓冲 `MemTable`（`RwLock<BTreeMap>` + `AtomicUsize` 字节计数；`put/delete`（墓碑）/`get`/`iter`/`range`；`MemEntry` 区分值/墓碑），`tests/lsm_memtable.rs` 覆盖覆盖写、墓碑、排序/范围、字节计数、并发写（`b336985`）。P7.3a 完成：`src/storage/lsm/coding.rs`（varint/fixed32）与 `block.rs`（LevelDB 式前缀压缩块 + restart 数组，`BlockBuilder`/`Block` 支持 `get` 二分 + 块内扫描、`first_key`、空块），`tests/lsm_block.rs` 覆盖往返/查找/边界/空块/编码截断（`45d1c6f`）。P7.3b 完成：`src/storage/lsm/sstable.rs`——`SSTableBuilder`（按块大小切分数据块，索引块记「块末 key → `BlockHandle`」，footer `index_offset/index_size/magic`）与 `SSTable`（解析索引、`get` 依索引二分选块、`iter` 顺序遍历、`first_key`），`tests/lsm_sstable.rs` 覆盖多块往返/命中未命中/单块/空表/损坏校验（`4bb82fc`）。P7.3c 完成：`src/storage/lsm/bloom.rs`（FNV-1a 双哈希，编码 `bit array + k`；空过滤器保守返回「可能」）并接入 SSTable footer（filter/index 两个 handle），`SSTable::get` 先过 bloom；`tests/lsm_bloom.rs` 覆盖无假阴性/假阳性率/空过滤器，`tests/lsm_sstable.rs` 加 bloom 接线用例（`dae2532`）。P7.4a 完成：`src/storage/lsm/store.rs`——`LsmStore`（memtable + 不可变 SSTable 列表；墓碑用 1 字节 tag 随 flush 落盘，`get` 新者优先、`iter` 归并去墓碑、`flush` minor、`compact` major 全量合并丢墓碑），`tests/lsm_store.rs` 覆盖覆盖写/多次 flush/墓碑跨层/合并保序（`f276b21`）。P7.4b 完成：`src/storage/lsm/persist.rs`——`PersistentLsm` 落盘为 `<dir>/MANIFEST`（写临时文件→fsync→rename 原子替换，列表即活跃 SSTable 文件号）+ `sst-NNNNNN.sst`；`open` 依 MANIFEST 恢复（孤儿文件忽略），`flush`/`compact` 先写文件再提交 manifest；`tests/lsm_persist.rs` 覆盖重开恢复/墓碑/新者胜/合并清理/孤儿忽略/未 flush 丢失（`e1ac224`）。**已知边界**：尚无 LSM WAL，未 flush 的 memtable 崩溃会丢（P7.4c/P7.5 补）。P7.5a 完成：`src/storage/lsm/engine.rs`——`LsmEngine: TableStorage`，以内部单调行号映射 `Rid` 为键（big-endian），值仍是 `creator/deleter/row` 版本化记录，故执行/索引/MVCC 调用方零改动；`insert/delete/delete_mark(返回 prev)/scan/get`，`flush/compact`，打开时按最大键续号；`tests/lsm_engine.rs` 覆盖生命周期/flush 后重开续号/合并保值（`105312a`）。**设计说明**：LSM 在此作为「按行号键的 KV」接入，`file_id` 返回保留哨兵 `LSM_FILE_ID`。P7.5b 完成：catalog `TableMeta` 增 `engine`（catalog 魔数升至 `CHIDCAT7`）；新建表引擎取 `storage.default_engine`（`Heap`/`Lsm`）；`Database::new_table_storage` 按引擎建堆文件或 `<n>.lsm` 目录；打开时按 engine 重建 `HeapEngine`/`LsmEngine`；`TableStorage` 增 `insert_at`（LSM 覆盖）与 `flush`（LSM 覆盖），`recover_from_wal` 按引擎路由（堆写页、LSM `insert_at`），`flush_inner` 先 flush LSM memtable 再截断 WAL，`drop_table` 对 LSM 删目录；`tests/lsm_db.rs` 覆盖 LSM 表 DML+checkpoint 重开、崩溃后 WAL 重放（`25617a5`）。**P7 完成**（`ENGINE=` SQL 语法未做，暂以配置选择引擎；LSM 键为内部行号）。 |
| P8 | LOB（外存 + `LobReader` 流式） | 🟡 P8.1 完成：`src/storage/lob.rs`——`LobStore`（每对象一个 `<id>.lob` 文件，id 打开时按现存最大文件续号、删除不复用；`write/read/len/is_empty/reader/delete`）与 `LobReader`（`read` 小缓冲 / `next_chunk` 64KB 流式）；`tests/storage_lob.rs` 覆盖空/小/1MB 往返、小缓冲流式、跨 chunk、删除、重开续号（`0d48d50`）。P8.2 待做：接入行编解码（超过 `storage.inline_lob_limit` 的字符串外存为 LOB 引用，解码时解析；更新/删除/vacuum 的 LOB 生命周期） |
| P9 | 多前端（MySQL/HTTP/Text TCP） | ⬜ |
| P10 | 并发：`ThreadHandler`（per-connection/thread-pool）+ 去全局锁 + 可配置冲突策略（FCW/2PL） | 🟡 P10.1：分库锁 + 去全局锁（`4984822`）+ `ThreadHandler` 双后端（`a1acc25`）。P10.2a：只读 autocommit 走本地快照事务，不写 `next_trx_id`/`open_trxs`/`committed_trxs`（`879c39e`）。P10.2b：`BufferPool` 元数据锁 + 每帧 `Mutex<PageData>`/`AtomicBool` 脏位，方法 `&self`（`f56694b`）。P10.2c：WAL 内部 `Mutex`（`f56c78d`）、Catalog `RwLock`（`9f8e801`）、事务簿记 `Atomic*`/`RwLock`（`64b52c1`）。P10.2d：`Database` 方法全 `&self`、读路径 `&Database`、`Instance` 每库 `RwLock<Database>`（读并发、写独占）（`2c70856`）。P10.3：`transaction.conflict = "fcw" | "2pl"`（`e068598`）。FCW：提交时按 `prev_deleter`/当前标记检测写写冲突并回滚失败方。2PL：每库悲观写锁（`Arc<DatabaseWriteLock>`，`Condvar` 等待 + `lock_timeout_ms`），显式事务在 `BEGIN`（取快照前）持锁至 `COMMIT/ROLLBACK`，自动提交写在语句内持锁；`Instance` 在取库锁前先取写锁，避免与 COMMIT 形成锁序死锁（`80596c1`）；`rollback_session` 释放。**P10 阶段完成** |

工作纪律：每步先写失败测试（红）再最小实现（绿），提交粒度对齐 chibicc（一次一件事），
提交前全量 `cargo test` + clippy 零警告，并同步本文档与 README。
