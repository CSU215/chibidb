# chibidb 交接文档（Handoff）

> 一份给下一个 Agent / 开发者的完整上下文。读完本文档即可在不了解前序对话的情况下继续开发。
> 最后更新：M16（VACUUM 空间回收 + flush 开事务守卫）完成后，252 个测试全绿，clippy 零警告，共 78 个提交。

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
cargo test                      # 全量回归（约 239 tests，20+ 个测试二进制）
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
| `exec.rs` | **最大的文件（~950 行）**：语句执行、表达式双上下文求值、聚合、分组、排序、JOIN、可见性过滤、EXPLAIN 计划 | `execute(db, trx, stmt)` |
| `value.rs` | `Value`：Null/Bool/Int(i64)/Float(f64)/Str/Date(i32 纪元天数)/Text | Display 决定 REPL 输出 |
| `datetime.rs` | 日期校验：civil-date 算法（Hinnant），`'YYYY-MM-DD'` 比较时隐式转日期 | `parse_date` |
| `trx.rs` | 事务：`Session` / `TrxState`（id+快照+undo）/ 可见性判定 / `Undo` | `TrxState::visible` |
| `result.rs` | `ResultSet::Message / Rows` | |
| `render.rs` | 对齐表格渲染（REPL 与 client 共用） | `write_result` |
| `repl.rs` | 本地 REPL（`db>` 提示符、exit/quit） | `run_repl` |
| `server.rs` | tokio TCP server，`Arc<Mutex<Database>>`，长度前缀协议，每连接一 Session | `serve(SharedDb, TcpListener)` |
| `client.rs` | TCP 客户端 | `run_client` |
| `wire.rs` | ResultSet/帧二进制编解码 | `encode_result_frame` / `decode_frame` |
| `catalog/mod.rs` | `Catalog`：`Table`/`HeapStore`/`IndexEntry`、`Schema`/`ColumnDesc`（带 `owner`）、`resolve()` 歧义检测 | |
| `catalog/meta.rs` | catalog.bin 自描述格式，魔数 **CHIDCAT4**（v4：含事务簿记 + 视图定义） | `CatalogSnapshot` |
| `storage/page.rs` | 页常量：`PAGE_SIZE=8192`、`FileId=u32`、`PageNo=u32`、`zeroed_page` | |
| `storage/disk.rs` | `DiskManager`：分页文件读写、建文件、魔数校验 | |
| `storage/buffer.rs` | `BufferPool`：64 帧、LRU `VecDeque`、脏页写回、Drop flush；`with_page(file,no,f)` 闭包式访问（访问即脏）、`read_page`（只读不脏） | |
| `storage/slotted.rs` | slotted 页纯函数：槽目录、变长条目、`page_insert`（空槽复用+压实）、`page_get/iter/delete/write` | |
| `storage/heap.rs` | `HeapFile`：page 0 文件头（魔数 **CHD2** v2 行带 MVCC 字段）、first-fit 多页、`insert/get/delete(物理)/delete_mark(MVCC)/for_each` | `Rid{page_no,slot}` |
| `storage/codec.rs` | 行/记录编码：自描述 tag（Null=00/Int=01/Float=02/Str=03/Bool=04/Date=05/Text）、值计数前缀；`encode_row/decode_row`；**版本化记录** `encode_record(creator,deleter,row)`（前 8 字节两个隐藏 u32） | |
| `index/key.rs` | 索引保序键编码：int 符号翻转大端、float 保序变换、str+NUL 结尾、date 符号翻转、null=0x00 | `encode_key` |
| `index/node.rs` | B+ 树节点页：叶/内部条目、lower/upper bound、bytes 占用阈值 25%、`page_write` 原位重写 | |
| `index/btree.rs` | B+ 树主体：`init/open/open_or_repair/at`、递归插入双级分裂长高、search（跨叶重复键回退）、scan_range 叶链、delete 借用/合并/根收缩（~880 行） | |
| `wal.rs` | 预写日志：帧 `[u32 len][u8 type][u32 trx][payload]`，Record::Insert/DeleteMark/Commit，追加 + `sync()`（提交点）+ `truncate()`（checkpoint）；`plan_recovery` 解析日志（容忍截断尾帧），纯函数有单测 | `Wal` / `plan_recovery` |

### 3.2 测试（`tests/`，22 个文件 / 239 tests）

- 与源码分层对应：`lexer / parser / eval / agg / join / db / db_index / db_persist / trx / wal / storage_* / index_* / wire / server / repl / datetime / codec / catalog_meta`
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

---

## 5. SQL 方言现状

```sql
-- DDL
CREATE TABLE t (id int, name char(10), score float, d date, body text);
CREATE INDEX idx_name ON t (col);
DROP INDEX idx_name;
DROP TABLE t;
CREATE VIEW v AS SELECT ...;               -- 定义以原 SQL 文本存入 catalog
DROP VIEW v;
-- DML
INSERT INTO t VALUES (1,'a',1.5),(2,'b',2.0);   -- 多值行，字面量允许负号，null 关键字
UPDATE t SET score = score + 1 WHERE id < 10;
DELETE FROM t WHERE name IS NULL;
-- 查询
SELECT [DISTINCT] * | expr [AS alias] (, ...)
  FROM tref (, tref)*                      -- 逗号 = cross join
  [JOIN | LEFT [OUTER] JOIN tref ON cond]* -- INNER / LEFT（未匹配左行右列补 NULL）
  [WHERE expr]
  [GROUP BY expr (, expr)*]
  [HAVING expr]
  [ORDER BY expr [ASC|DESC] (, ...)*]      -- NULL 升序在前降序在后，稳定排序
  [LIMIT n [OFFSET m]]
-- 聚合：count(*)/count(x) 忽略 null / sum / avg（恒 float）/ min / max；空集 sum/avg/min/max → NULL
-- 表达式：+ - * /（int 截断、checked overflow）、and/or/not（SQL 三值逻辑）、
--         = <> < <= > >=、is [not] null、括号、expr [NOT] IN (值列表)、
--         expr [NOT] IN (SELECT..)（单列）、[NOT] EXISTS (SELECT..)、标量 (SELECT..)
-- 事务
BEGIN; COMMIT; ROLLBACK;
CHECKPOINT;                                -- flush_all + save_catalog + 截断 wal；有开事务时报错
VACUUM;                                    -- 物理回收（见 §6.4）；有开事务时报错
EXPLAIN SELECT ...;                        -- 输出 FullScan / IndexScan / NestedLoopJoin
```

子查询/视图的实现要点（改相关代码前必读）：

- `IN (值列表)` 在 **parser 里脱糖**成 OR/AND 比较链，三值语义免费正确（列表含 NULL 时 NOT IN 永不 TRUE）
- 子查询（InSubquery/Exists/ScalarSubquery）走 `exec::lift_subqueries`：execute_select 入口先把整棵 select 表达式树里的子查询**执行一次并改写为 `Expr::Value` 字面量**，eval 本身保持无上下文；嵌套子查询自然递归；**只支持不相关子查询**（引用外层列会报 no such column）
- 标量子查询：0 行 → NULL，>1 行或 >1 列报错；IN 子查询要求单列
- 视图：定义 SQL 原文存 catalog（parser 用 token 偏移切片，需 `Parser.src`）；查询时 `exec::from_source` 在 FROM 处展开（真实表走 MVCC，视图递归执行其 select）；视图列 dtype 用 Text 占位（查询路径不用 dtype）；视图无索引（find_sargable 直接跳过）；CREATE VIEW 时试执行一次做校验（表/列存在性）
- catalog 格式已升 **CHIDCAT4**（views 字段）；视图名与表名互斥占用

细节语义（已被测试锁定，不要随意改）：

- 标识符大小写不敏感（表名/列名/关键字），表名与列名大小写敏感匹配保持现状
- 整数除法向零截断；除零报错；`1/0=2` 三值逻辑：NULL 参与比较 → NULL（UNKNOWN），WHERE 只放行 TRUE
- `date` 列插入字符串时严格按 `YYYY-MM-DD` 校验（闰年用 Hinnant 算法）；与字符串比较时隐式转换
- `char(n)` 按字符数校验；`text` 无长度限制但单行记录超 8KB 报错
- 聚合无 GROUP BY 时裸列出现在 SELECT 会报错；GROUP BY 下其它列取首行（宽松语义）
- ORDER BY 可引用 SELECT 别名（如 `count(*) as total ... order by total`）
- JOIN 中同名非限定列报 `ambiguous column`，用 `alias.col` 限定
- LIMIT 只接受非负整数字面量
- 显式事务内执行 DDL 报错（`DDL inside a transaction is not supported`）

---

## 6. 存储与 MVCC 设计（继续动这里前必读）

### 6.1 文件布局

```
<data_dir>/
  catalog.bin            # 魔数 CHIDCAT3 + next_table_file/next_index_file/next_trx_id/committed[]
                          # + 表元数据（列定义+file_no）+ 索引元数据（name/table/column/file_no）
  wal.bin                # 预写日志（见 §6.3）；干净关闭/flush 后为 0 字节
  tables/000000.dbf ...  # 每表一个 HeapFile；page 0 头魔数 CHD2，数据页从 1 起，first-fit
  indexes/000000.idxf ...# 每索引一个 B+ 树文件；page 0 头魔数 CHIDBTX1
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
- 提交：`committed_trxs.insert(id)` 后立即 `save_catalog()`（提交持久化点）
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

- 无 RIGHT/FULL OUTER JOIN、无 UNION、无 ALTER TABLE、无相关子查询（子查询引用外层列）、无视图上的 INSERT/UPDATE（视图只读）
- 无 MOD/% / 字符串函数；无 LIKE（除 IS NULL 外）
- DISTINCT + ORDER BY 引用非投影列时，保留哪一行是按扫描顺序首个（标准 SQL 视为非法，未做校验）
- VACUUM 回收的空页不归还文件系统（页留给 first-fit 复用）；空页只在该表变小时浪费
- 单 Mutex 单 writer 串行化；无死锁检测；vacuum 之外长事务 + 未提交孤儿版本仍会占空间
- BufferPool 无预读；WAL checkpoint 是全量截断（有预算护栏但无模糊检查点）；first-fit 插入是 O(页数)
- **多连接下的 DDL 隔离不存在**：一个连接持有未提交事务时，另一连接仍可 DROP TABLE / CREATE INDEX / DROP VIEW（open_trxs 注册表只护 WAL 截断与 VACUUM，不锁元数据）；教学场景可接受，修法需先做会话级元数据锁
- `exec.rs` ~1200 行偏大，未来可拆 eval/aggregate/join/plan/subquery 五个模块

---

## 9. 建议的后续顺序（已确认的执行计划，按序执行）

1. **LIKE 匹配**（小）：lexer 补 `%` 标点（顺带解锁 MOD 运算符，一次 lexer 改动两用）；parser 加 `expr [NOT] LIKE 'pattern'`；执行器写 `%`/`_` 通配的简单匹配器；**不支持转义**（方言文档注明）；不做索引下推
2. **字符串函数**（中）：AST 加 `Expr::Function(name, args)`，先做 CONCAT/UPPER/LOWER/LENGTH/SUBSTRING；eval 保持纯函数，与子查询物化机制互不干扰
3. **exec.rs 拆分**（中，纯重构）：~1200 行拆成 eval/aggregate/join/plan/subquery 五个模块；趁功能面稳定时做，之后每项改动的成本都会下降
4. **miniob 兼容性回归用例移植**（测试）：把 miniob 经典测试场景（CRUD、聚合、join、子查询组合拳）转成集成测试，锁定方言行为
5. **基准测试**（小）：B+ 树点查/范围扫 vs 全表扫的对比计时，README 附数字；顺带验证 3 的重构无回归
6. **相关子查询**（大，压轴）：eval 传入外层行上下文（EvalCtx 链式），子查询物化改为逐行缓存；复杂度高、教学收益相对低

理由：1+2 补齐 SQL 表达式面且互相搭车；3 在 4/6 之前做，减少测试改动打架；5 给 README 增色并验证重构。

提交基线：`975dab2 feat: vacuum ...`（HEAD）。
