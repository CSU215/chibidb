# chaoticdb 交接文档（Handoff）

> 给下一个开发者/Agent 的上下文。读完本文档即可在不了解前序对话的情况下继续开发。
> 数字会随提交变化，以 `cargo test --workspace` 实际输出为准。

## 1. 项目是什么

纯 Rust + tokio 手写的教学型单机关系数据库。按 **TDD 红绿**节奏演化，提交细粒度。
存储、索引、事务、执行器、网络前端全部手写。当前 `cargo test --workspace` 为
**704 passed + 9 ignored**（探针），`ffi` 子 crate 把引擎编译成 `cdylib`。

已完成的主干：SQL → lexer → parser(AST) → LogicalOperator（翻译 + 优化，`exec/planner/`）
→ PhysicalOperator（lower，`exec/operator/`，对 Statement 无知）→ 执行器（chunk/volcano）
→ B+ 树索引 → PG 式 MVCC（snapshot + clog + 行锁 + SSI）→ WAL 崩溃恢复
→ 堆/LSM 双引擎 → BufferPool/DiskManager → 多库 Instance → Text/HTTP/MySQL 三个前端。

## 2. 环境与命令

- Rust stable（edition 2024），Windows 11 + PowerShell 5.1；代码跨平台、无平台特定 API。
- 直接依赖：`tokio`、`parking_lot`、`serde`、`toml`、`thiserror`、`sha2`、`tempfile`。

```powershell
cargo test --workspace           # 全量回归（含 ffi，约数十秒）
cargo test --test trx            # 单个测试文件
cargo clippy --workspace         # 提交前清零
cargo run -q                     # 内存实例 REPL（临时目录后端）
cargo run -q -- <dir>            # 文件实例 REPL
cargo run -q -- serve <dir> [addr]   # TCP server（addr 缺省取 config.toml）
cargo run -q -- client [addr]    # 交互式客户端
python scripts\smoke.py          # 起 server→SQL→强杀→复开验证 WAL，期望 SMOKE OK
```

启动时从**当前工作目录**读 `config.toml`（缺失即默认值，未知键报错）。键见
`config.example.toml`；`admin_api` 受 `config.rs` 支持但示例文件未列出。

## 3. 代码地图

### 3.1 源码（`src/`）

| 文件 | 职责 | 关键类型/入口 |
|---|---|---|
| `main.rs` | REPL / `serve` / `client` 子命令 | `main` |
| `lib.rs` | 门面：模块声明与 `pub use` 重导出 | `Database` / `ResultSet` / `Value` / `client` / `server` |
| `instance.rs` | 单实例多库：数据根 + `chibi_meta` + `information_schema`；用户/权限 | `Instance::open` / `execute_with` / `database` |
| `config.rs` | 配置中心（storage/wal/server/execution/auth/transaction/observability） | `Config::load` / `from_toml_str` / `validate` |
| `value.rs` | `Value`（Null/Bool/Int/Float/Str/Date）与 `DataType`（int/float/char(n)/date/text） | |
| `error.rs` | `Error`（Syntax 带字节位置 / Runtime / Retry）与 `Result` | |
| `sql/lexer.rs` | 分词 | `lex(src)` |
| `sql/parser.rs` | 递归下降解析全部 SQL | `parse(sql) -> Vec<Stmt>` |
| `sql/ast.rs` | AST；`Stmt::is_read_only()` / `needs_exclusive()` 决定路由与锁粒度 | `Stmt` / `Expr` / `SelectStmt` |
| `sql/result.rs` | `ResultSet::Message/Rows/Affected` | |
| `sql/datetime.rs` | 日期校验/格式化（Hinnant civil-date） | `parse_date` |
| `exec/mod.rs` | 语句分发：DDL/SHOW/EXPLAIN/CHECKPOINT/VACUUM；DML 的 `apply_*`、EPQ 重试、`coerce`；统一执行缝 `collect_rows` | `execute(db, trx, stmt)` |
| `exec/command.rs` | 翻译期绑定好列下标的 DML 命令：`InsertCommand`/`UpdateCommand`/`DeleteCommand` | `*Command::resolve` |
| `exec/dml.rs` | 命令算子 `InsertOp`/`UpdateOp`/`DeleteOp`（持命令、`output_kind = Command`） | |
| `exec/planner/` | 规划层：`logical`（LogicalOperator + 翻译/优化：谓词下推、WHERE→JOIN）、`lower`（访达路径选择、`*`/别名展开、聚合重写）、`access`（索引选路）、`fold`（常量折叠）、`explain`、`util`（纯 AST 助手） | `plan_statement` / `plan_select` |
| `exec/operator/` | 火山算子（对 Statement 无知）：`PhysicalOperator` + `ExecContext{db,trx,outer}`；`basic`/`scan`/`index_scan`/`join`/`subquery`（`PlannedSubqueries` 承载计划期预降的子查询） | `physical_tree` |
| `exec/chunk.rs` | 列式批处理 `Chunk`/`Column`（`CHUNK_ROWS=1024`）与行式桥接 | |
| `exec/eval.rs` | 表达式求值（三值逻辑、LIKE+ESCAPE、标量函数、`EvalCtx` 父链） | `eval_const` / `eval_bound` |
| `exec/aggregate.rs` | 聚合、GROUP BY/HAVING、ORDER BY、DISTINCT 的共享逻辑；`resolve_order_aliases` | |
| `exec/subquery.rs` | IN/EXISTS/标量子查询改写；优先执行计划期建好的子计划，回退才现场规划 | `bind_expr` / `eval_bound` |
| `catalog/mod.rs` | `Catalog`/`Table`/`Schema`/`ColumnDesc`/`IndexEntry`；`resolve` 歧义检测 | `Catalog::table` / `create_table` / `create_index` |
| `catalog/meta.rs` | `catalog.bin` 自描述格式（含事务簿记/约束/视图） | `encode_catalog` / `decode_catalog` / `CatalogSnapshot` |
| `storage/page.rs` | `PAGE_SIZE=8192`、`FileId=u32`、`PageNo=u32` | |
| `storage/disk.rs` | 分页文件 IO；按文件加锁（注册表 `RwLock` + 每文件 `Mutex`）；DWB 接线 | `DiskManager::with_file` / `alloc_page` |
| `storage/buffer.rs` | `BufferPool`：帧、pin、可配置淘汰、脏页写回、`PoolStats`/`CacheReporter` | `with_page` / `read_page` / `flush_all` |
| `storage/replacer.rs` | 淘汰策略接缝：`lru`（默认）/`clock`/`fifo` | `from_policy` |
| `storage/slotted.rs` | slotted 页纯函数（槽目录、变长条目、插入/读取/删除） | |
| `storage/engine.rs` | `TableStorage` 接缝 + `HeapEngine`（流式扫描） | `insert` / `delete_mark` / `insert_at` |
| `storage/heap.rs` | `HeapFile`：文件头、first-fit、Rid、物理删除、`open_or_repair` | `Rid{page_no,slot}` |
| `storage/codec.rs` | 行编码与版本化记录 `[creator u64][deleter u64][next_rid u64][row]` | `encode_record` / `decode_record` |
| `storage/pax.rs` | PAX 列式页（只读所需列） | |
| `storage/lob.rs` | 外存大对象（超 `inline_lob_limit` 的字符串） | `LobStore` / `LobReader` |
| `storage/lsm/*` | `memtable` / `sstable`（块 + bloom + footer）/ `store`（分级压实）/ `persist`（MANIFEST）/ `engine`（`LsmEngine: TableStorage`） | |
| `index/key.rs` | 保序字节键编码（int 符号翻转、float 保序、str、date、null） | `encode_key` |
| `index/node.rs` | B+ 树节点页（叶/内部、lower/upper、字节阈值） | |
| `index/btree.rs` | B+ 树主体：`init/open/open_or_repair/at`、插入分裂、`search`、`scan_range`、删除借用/合并 | `BTree` |
| `txn/mod.rs` | 事务层重导出 | |
| `txn/clog.rs` | `CommitStatus`：分段 `AtomicU64` 提交位图 + 真空地平线 `base` | `mark_committed` / `is_committed` / `advance_base` |
| `txn/transaction.rs` | `Snapshot{xmax,xip}` + `TransactionManager`（id 分配、open 集、commits 计数、SSI 生命周期） | `begin_open` / `snapshot` / `commit` |
| `txn/lock.rs` | 行级锁：`(table,rid)` FIFO、超时、等待图死锁检测 | `lock` / `unlock_all` |
| `txn/ssi.rs` | 简化 SSI：表粒度读写集 + rw-反依赖边 + 提交期环检测 | `Ssi` |
| `txn/trx.rs` | `Session`（trx/current_db/user/autocommit）+ `TrxState`（id/snapshot/clog/undo/wal/explicit）+ `Undo` | `TrxState::visible` |
| `db/mod.rs` | `Database` 门面：打开/重开、`execute_stmt_with`、`resolve_optimize_execute`、commit/conflict、`collect_plan` | `Database::open` / `execute_sql` |
| `db/store.rs` | 存储/catalog/回滚：`store_insert`/`store_delete_mark`/`store_update_versions`、`save_catalog`、`drop_table`、`undo_to` | |
| `db/recovery.rs` | WAL 重放 + 被触及表的索引重建 | `recover_from_wal` |
| `db/checkpoint.rs` | `flush`/`flush_inner`（checkpoint）与 `vacuum` | |
| `db/unique.rs` | PK/UNIQUE 检查：按索引键加锁 + 以已提交状态判定 + `next_rid` 链排除同逻辑行 | `check_unique` |
| `wal.rs` | WAL 帧 `[u32 len][u8 type][u64 trx_id][payload]`，Record::Insert/DeleteMark/Commit | `Wal` / `plan_recovery` |
| `net/server.rs` | tokio accept + 阻塞线程处理（`per-connection`/`thread-pool`） | `serve` |
| `net/client.rs` / `net/repl.rs` / `net/render.rs` | 客户端 / 本地 REPL / 表格渲染 | |
| `net/protocol.rs` / `net/wire.rs` | 前端编解码接缝与帧编解码 | |
| `net/http.rs` / `net/admin.rs` / `net/json.rs` | HTTP `/health`、`/query`、`/session`；`/api/parse` 与静态托管；手写 JSON | |
| `net/session.rs` | HTTP 会话注册表（`X-Chibi-Session`） | |
| `net/mysql.rs` | MySQL wire：握手（`mysql_native_password`）、文本查询、预处理语句 | |
| `ffi/src/lib.rs` | C ABI：`chaoticdb_open/exec/query/free/close/last_error` | |

### 3.2 测试（`tests/`）

约 60 个文件、700 个用例，按层分布（`lexer/parser/eval/agg/join/union/db*/trx/wal/vacuum/
storage_*/index_*/lsm_*/wire/server/http_*/mysql_frontend/instance/users/privileges/
information_schema/constraints/correlated/miniob_compat/engine_equivalence/concurrency*`）。

- `tests/db.rs` 的 `with_dbs` 模式让同一用例在内存与文件后端各跑一遍。
- `tests/wal.rs`、`tests/vacuum.rs` 用 `Database::simulate_crash()` 跳过缓冲池干净落盘来模拟被杀。
- `tests/bench.rs`（6 个）与 `tests/perf_stats.rs`（3 个）是 `#[ignore]` 性能探针。
- `tests/engine_equivalence.rs` 对 heap/LSM 做固定种子差分等价；`tests/concurrency_fuzz.rs`
  做随机并发调度断言余额守恒与主键唯一。

## 4. MVCC / WAL / VACUUM 设计（改这里前必读）

完整版见 [`MVCC.md`](MVCC.md)，此处只列要点：

- **可见性**：版本头 `creator/deleter/next_rid`；`TrxState::visible` 判定
  `creator ∈ {0,self,snapshot}` 且 `deleter ∉ {self,snapshot}`。快照是 `{xmax, xip}`，
  提交由 clog 位图回答，`xid 0` 为只读哨兵。
- **DML**：INSERT 追加新版本；DELETE 原位 `delete_mark`（不物理回收）；UPDATE = 标记旧版
  + 追加新版并把旧版 `next_rid` 指向新版（PG `t_ctid`），索引项只追加、读时按可见性过滤。
- **提交**：只写事务才追加帧 + `wal.sync()`（提交点）+ 标记 clog；不每提交重写 catalog
  （catalog 由 DDL/checkpoint 保存）。WAL 超 `checkpoint_threshold` 且无开事务时机会式 checkpoint。
- **恢复**：只重放带 Commit 的事务，按提交顺序；Insert 仅在目标槽空时原位写回，DeleteMark
  条件重放；被触及的表重建索引（索引是派生数据，B+ insert 不幂等）。
- **checkpoint**：`flush_all` → 各 LSM 表 flush → `save_catalog` → 若期间无提交且无开事务则
  `wal.truncate`。`flush()` 有全局开事务守卫；`flush_inner` 供 CHECKPOINT 排除自身临时事务。
- **vacuum**：无开事务时，物理删除「creator 未提交」的孤版本与「deleter 已提交」的删除标记行
  （含 stale 索引项），把未提交 deleter 的标记清成未删除，然后推进 clog 地平线到 `next_id`
  并持久化，clog/committed 集合只保留地平线之上的提交。

## 5. 并发模型

- **行级锁**：`LockManager` 以 `(table, rid)` 为键，FIFO 交给下一个等待者，持至事务结束；
  等待超时报 `lock wait timeout`，超过 `deadlock_timeout_ms`（构造参数 100ms）做等待图检测，
  成环即回滚该等待者。
- **EPQ（仅 RC）**：拿到行锁后若该版本被「本快照之后提交」的事务改动过，返回内部
  `Error::Retry`；`exec::epq_retry` 回滚本语句（`rollback_statement`，**不释放行锁**）后刷新
  快照重跑整条语句，最多 `MAX_EPQ_RETRIES=16` 次。
- **RR / Serializable**：不重启；提交期 `check_conflicts`（写写冲突）或 `ssi_conflict`
  （读写依赖成环）报 40001。
- **唯一约束**：按索引键加锁（复用行锁管理器）后以已提交状态判定；`next_rid` 链用于把
  「同逻辑行的新版本」排除出重复判定。
- **DDL 隔离**：其它会话有开事务时拒绝 CREATE/DROP（`schema is locked by an open transaction`）。

## 6. 工作约定

1. **TDD 红绿**：一个能力点 = 红测试 → 最小实现 → 全量绿 → 一个细粒度提交。
2. **提交前**：`cargo test --workspace` 全绿 + `cargo clippy --workspace` 零警告。
3. 提交信息用 Conventional Commits（`feat:` / `fix:` / `refactor:` / `chore:` …）。
4. 纯函数层（lexer/node/slotted/codec/key）直接单测；系统行为写 `tests/db*.rs` 风格集成测试。
5. 旧数据文件格式变更要 bump 魔数并明确报错，不要静默误读。
6. 不随意引入新依赖；改动分层内聚，避免只打补丁的大重构。
7. PowerShell 5.1：不要用默认编码写含中文的 Rust 源文件（会非法 UTF-8）；不要用管道后的
   `$?`/`$LASTEXITCODE` 判断 cargo 成败，分两条命令显式检查；`cargo test` 给足超时。

## 7. 已知边界 / 技术债

- 无 `ALTER TABLE`、无 `FULL OUTER JOIN`；视图只读，不能对视图 INSERT/UPDATE，视图无索引。
- 相关与不相关子查询都是**逐外层行物化、无缓存**；正确但非最优。
- 索引访问路径只做**单列**：AND 链里同列上下界合并为一段范围扫，跨列不合并；同一列多个
  下界/上界只保留一个。
- 排序消除只覆盖「单表 + 索引扫描 + ORDER BY 单列 ASC = 该索引列」（EXPLAIN 报
  `OrderedIndexScan`），DESC 与多列/异列仍排序。
- checkpoint 是**全量 WAL 截断**，无模糊检查点；日志未压缩。
- vacuum 释放的空页不归还文件系统（留给 first-fit 复用）。
- `BufferPool` 的页查找与淘汰共用一把 `state` 锁，一次缺页/淘汰会阻塞所有页查找；
  `DiskManager` 已按文件加细锁。
- 行锁持至事务结束、无锁升级；RR/Serializable 冲突直接 abort 而非重读。
- 命名遗留：HTTP 会话头仍为 `X-Chibi-Session`，系统库目录为 `chibi_meta`（兼容既有配置/客户端）。
- 内置演示前端在 `web/demo/`（纯 HTML/CSS/JS，`include_str!` 内嵌，无构建），
  `[web] enabled = true` 时在 `/` 托管并优先于 `server.web_root`（默认 `web/dist`）；
  没有 Vue/`scripts/build_web.sh`，`web_root` 仅用于外置构建产物。
  `/api/*` 在 `[web] enabled` **或** `server.admin_api` 开启后可用；`/api/files`、`/api/page`
  另需 `[web] page_preview = true`（读原始磁盘页）。内嵌控制台源码改动需重新编译（`include_str!`）。
- `config.example.toml` 未列出 `server.admin_api`（代码已支持）。

