# chibidb 交接文档（Handoff）

> 一份给下一个 Agent / 开发者的完整上下文。读完本文档即可在不了解前序对话的情况下继续开发。
> 最后更新：M12.1（MVCC 核心）完成后，211 个测试全绿，共 57 个提交。

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
cargo test                      # 全量回归（约 211 tests，20+ 个测试二进制）
cargo test --test trx           # 单个测试文件
cargo test --quiet              # 安静模式（注意配合退出码判断，见 §8）
cargo build
cargo run -q                    # 内存数据库 REPL（实际是临时目录，见 §7）
cargo run -q -- <dir>           # 文件数据库 REPL
cargo run -q -- serve <dir>     # TCP server，监听 127.0.0.1:5678
cargo run -q -- client [addr]   # 交互式客户端
```

冒烟演示（PowerShell 后台起 server）：

```powershell
$job = Start-Job -ScriptBlock { Set-Location E:\WorkBench\MiniProjects\chibidb; cargo run -q -- serve tmp\demo }
Start-Sleep -Seconds 3
"create table t (id int, name char(10));
insert into t values (1, 'alice');
explain select * from t where id = 1;
create index idx on t (id);
explain select * from t where id = 1;
select * from t where id = 1;
exit" | cargo run -q -- client
Stop-Job $job; Remove-Job $job -Force
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
| `catalog/meta.rs` | catalog.bin 自描述格式，魔数 **CHIDCAT3**（v3：含事务簿记） | `CatalogSnapshot` |
| `storage/page.rs` | 页常量：`PAGE_SIZE=8192`、`FileId=u32`、`PageNo=u32`、`zeroed_page` | |
| `storage/disk.rs` | `DiskManager`：分页文件读写、建文件、魔数校验 | |
| `storage/buffer.rs` | `BufferPool`：64 帧、LRU `VecDeque`、脏页写回、Drop flush；`with_page(file,no,f)` 闭包式访问（访问即脏）、`read_page`（只读不脏） | |
| `storage/slotted.rs` | slotted 页纯函数：槽目录、变长条目、`page_insert`（空槽复用+压实）、`page_get/iter/delete/write` | |
| `storage/heap.rs` | `HeapFile`：page 0 文件头（魔数 **CHD2** v2 行带 MVCC 字段）、first-fit 多页、`insert/get/delete(物理)/delete_mark(MVCC)/for_each` | `Rid{page_no,slot}` |
| `storage/codec.rs` | 行/记录编码：自描述 tag（Null=00/Int=01/Float=02/Str=03/Bool=04/Date=05/Text）、值计数前缀；`encode_row/decode_row`；**版本化记录** `encode_record(creator,deleter,row)`（前 8 字节两个隐藏 u32） | |
| `index/key.rs` | 索引保序键编码：int 符号翻转大端、float 保序变换、str+NUL 结尾、date 符号翻转、null=0x00 | `encode_key` |
| `index/node.rs` | B+ 树节点页：叶/内部条目、lower/upper bound、bytes 占用阈值 25%、`page_write` 原位重写 | |
| `index/btree.rs` | B+ 树主体：`init/open/at`、递归插入双级分裂长高、search（跨叶重复键回退）、scan_range 叶链、delete 借用/合并/根收缩（~850 行） | |

### 3.2 测试（`tests/`，21 个文件 / 211 tests）

- 与源码分层对应：`lexer / parser / eval / agg / join / db / db_index / db_persist / trx / storage_* / index_* / wire / server / repl / datetime / codec / catalog_meta`
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
| M11 子查询/视图 | ❌ **未开始** | |
| M12 事务 | 🟡 **进行中**：M12.1 MVCC 核心已完成（`568f62d`）；**M12.3 WAL+崩溃恢复未做**（M12.2 的 update/delete MVCC 化已并入 M12.1） | `568f62d` |
| M13 TCP server + client + wire 协议 | ✅ | `d06103c` |
| M14 加固 | ❌ 未开始（DROP TABLE、README、基准等） | |

---

## 5. SQL 方言现状

```sql
-- DDL
CREATE TABLE t (id int, name char(10), score float, d date, body text);
CREATE INDEX idx_name ON t (col);
DROP INDEX idx_name;
-- DML
INSERT INTO t VALUES (1,'a',1.5),(2,'b',2.0);   -- 多值行，字面量允许负号，null 关键字
UPDATE t SET score = score + 1 WHERE id < 10;
DELETE FROM t WHERE name IS NULL;
-- 查询
SELECT [DISTINCT 未实现] * | expr [AS alias] (, ...)
  FROM tref (, tref)*                      -- 逗号 = cross join
  [JOIN tref ON cond]*                     -- 仅 INNER（left/right/outer 未实现，遇到关键字会报错）
  [WHERE expr]
  [GROUP BY expr (, expr)*]
  [HAVING expr]
  [ORDER BY expr [ASC|DESC] (, ...)*]      -- NULL 升序在前降序在后，稳定排序
  [LIMIT n [OFFSET m]]
-- 聚合：count(*)/count(x) 忽略 null / sum / avg（恒 float）/ min / max；空集 sum/avg/min/max → NULL
-- 表达式：+ - * /（int 截断、checked overflow）、and/or/not（SQL 三值逻辑）、
--         = <> < <= > >=、is [not] null、括号；字符串连接未实现；%/^/位运算未实现
-- 事务
BEGIN; COMMIT; ROLLBACK;
EXPLAIN SELECT ...;                        -- 输出 FullScan / IndexScan / NestedLoopJoin
```

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

### 6.3 下一站：M12.3 WAL + 崩溃恢复（**这是建议的下一个任务**）

当前缺口：进程被杀（BufferPool 的 Drop flush 没跑）时，已 COMMIT 的数据可能只在脏页没进文件 → 丢数据。catalog 在 commit 时落盘，但数据页没有。

建议设计（与现有代码契合度最高的方案）：

1. 新模块 `src/wal.rs`：单个 `wal.bin` 追加日志，记录帧 `[u32 len][u8 type][u32 trx_id][payload]`
   - `INSERT`：file_no + Rid + 完整版本化记录字节
   - `DELETE_MARK`：file_no + Rid + deleter
   - `UPDATE` 可直接记为 DELETE_MARK+INSERT 两条（或新类型）
   - `COMMIT`：trx_id；`BEGIN/ROLLBACK` 可选
2. 写时机：与 undo 同处追加（在 `lib.rs` 的 store_insert/store_delete_mark/store_update_versions 内），**COMMIT 时 fsync 日志**（tokio 无 fsync——存储层目前全同步 std::fs，用 `File::sync_all()`）
3. 恢复（`Database::open` 末尾）：
   - 顺序读 wal，按 trx 收集；只有带 COMMIT 记录的事务做 REDO：
     - INSERT：仅当目标 Rid 当前为空槽（页不存在/槽空）才重放，避免重复
     - DELETE_MARK：rid 存在且 deleter==0 时打标记
   - 未提交事务的插入物理保留但天然不可见（creator 不在任何快照）；其删除标记也不生效（deleter 未提交）→ **只需 REDO 已提交**
   - catalog 的 committed_trxs 已经在 COMMIT 时持久化，恢复后可见性正确
4. 干净关闭（flush 成功后）truncate wal.bin（checkpoint 简化版）
5. 测试（新建 `tests/wal.rs`，红→绿）：
   - 提交后模拟崩溃：需要一个绕过 Drop flush 的退出路径——给 Database 加 `simulate_crash(self)`（`std::mem::forget(self)` 或显式标记阻止 Drop）
   - 重开：已提交插入/删除生效；未回滚事务的数据不可见
   - 截断半个尾部帧时恢复不报错（忽略不完整帧）
6. 完成标准：`cargo test` 全绿，提交 `feat: wal redo recovery`

---

## 7. 关键约定与工作流（新 Agent 必须遵守）

1. **TDD 节奏**：一个能力点 = 一个/几个红测试 → 最小实现 → `cargo test` 全绿 → 一个细粒度提交。提交消息用 Conventional Commits：`feat: ...` / `fix: ...` / `refactor: ...` / `chore: ...`
2. **提交前必须看到全量测试通过**。不要把编译错误/红灯混进提交（项目历史里有过两次，随后紧跟 fix 提交，尽量避免）
3. 测试与实现都放在 `tests/` 与 `src/` 既有分层中；纯函数层（lexer/node/slotted/codec/key）直接单测，系统行为写集成测试（`tests/db*.rs` 风格）
4. 保持向后兼容的取舍：旧数据文件格式变更要 bump 魔数（heap 已 CHD2，catalog 已 CHIDCAT3，btree CHIDBTX1）
5. `open_in_memory()` 实际是 `tempfile::TempDir` 后端（M9 时统一的，避免双后端分叉），Database 持有 `_temp: Option<TempDir>` 自动清理
6. 大的重构优先于打补丁：如 M9 把 Mem/Heap 双存储统一成 Heap-only（`f13f83c`），可参考其风格
7. 不要引入新依赖，除非确有必要（目前仅 3 个直接依赖）

### PowerShell 5.1 踩坑记录（血泪）

- **不要用 `Set-Content` 写 Rust 源文件**：默认编码非 UTF-8，会把中文注释写成非法字节导致 rustc 报 "stream did not contain valid UTF-8"。必须写文件时用 `[System.IO.File]::WriteAllText($path, $content, [System.Text.UTF8Encoding]::new($false))`，或直接用编辑工具
- **不要靠 `if ($?)` 串联 cargo 命令做提交判断**：管道后 `$?` 反映的是最后一个 cmdlet（如 Select-String），不代表 cargo 成功，曾导致红灯被提交或提交悄悄没执行。正确做法：分两条命令，先看测试输出，再显式 `git add; git commit`
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

- 无 LEFT/RIGHT/FULL OUTER JOIN、无子查询（IN/EXISTS/标量子查询）、无视图、无 DISTINCT/UNION、无 DROP TABLE、无 ALTER TABLE
- 无 MOD/% / 字符串函数；无 LIKE（除 IS NULL 外）
- MVCC 删除标记与 stale 索引项不做物理回收（页面会持续膨胀）
- 单 Mutex 单 writer 串行化；无死锁检测；长事务 + 未提交孤儿版本会长期占空间
- BufferPool 无预读/检查点；first-fit 插入是 O(页数)
- WAL 未实现（见 §6.3）；目前仅 catalog 在 COMMIT/DDL 时落盘
- server 异常断连时的事务回滚需要复核接入点
- `exec.rs` ~950 行偏大，未来可拆 eval/aggregate/join/plan 四个模块
- 无基准测试、无 README（M14 时写）

---

## 9. 建议的后续顺序

1. **M12.3 WAL + 崩溃恢复**（§6.3 详细方案）——补上事务最后一块
2. **M14 加固**：DROP TABLE（连索引一起清）、README、全链路冒烟脚本、`cargo clippy` 清零
3. **M11 子查询/视图**（时间允许）：`WHERE x IN (SELECT ...)` / `EXISTS` / `CREATE VIEW`
4. 可选加分项：DISTINCT、LEFT JOIN、字符串函数、miniob 兼容性回归用例

提交基线：`568f62d feat: mvcc transactions with snapshot isolation and rollback`（HEAD）。
开工第一步：`cargo test` 确认 211 全绿，然后从 §6.3 的 WAL 红测试开始。
