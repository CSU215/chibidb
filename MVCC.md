# chaoticdb MVCC 与并发控制（as-built）

本文记录**当前已实现**的 MVCC/并发设计，不是提案。实现全面以 PostgreSQL 为模板：
PG 式快照、clog 提交状态、行级锁、EPQ、RR/Serializable 的 40001、简化 SSI、版本链与真空地平线。

配置入口（`src/config.rs`）：

```toml
[transaction]
isolation = "read_committed"   # read_committed（默认）| repeatable_read | serializable
lock_timeout_ms = 5000          # 行锁等待上限；死锁检测超时固定 100ms
```

隔离级别作用于整个 `Database`，不做会话级 `SET TRANSACTION`。

---

## 1. 版本记录与可见性

每条堆记录（`src/storage/codec.rs`）在行数据前有 24 字节版本头：

```
[creator u64][deleter u64][next_rid u64][行 codec]
```

- `creator`：插入该版本的事务 id，0 = 无（只读哨兵路径不会写）。
- `deleter`：删除/更新该版本的事务 id，0 = 仍存活。
- `next_rid`：UPDATE 时旧版本指向新版本的前向指针（PG `t_ctid`），用 `pack_rid`/`unpack_rid`
  在 `u64` 里编码 `(page_no, slot)`；`0` = 无后继。

`TrxState::visible`（`src/txn/trx.rs`）：

```rust
creator_visible = creator == 0 || creator == self.id || committed_before(creator);
deleted_for_me  = deleter != 0 && (deleter == self.id || committed_before(deleter));
visible         = creator_visible && !deleted_for_me;
```

即：自己写的行对自己可见；自己删的行对自己立即消失；其余按「快照之前是否已提交」判定。

## 2. PG 式快照

`src/txn/transaction.rs`：

```rust
pub struct Snapshot { pub xmax: u64, pub xip: Vec<u64> }
```

- `xmax` = 取快照时的 `next_id`；`xid >= xmax` 一律不可见（快照后才分配）。
- `xip` = 取快照时仍在途的 xid 列表（升序）。

判据（`TrxState::committed_before`）：

```rust
xid < snapshot.xmax && !snapshot.xip.contains(&xid) && clog.is_committed(xid)
```

`TransactionManager::snapshot()` 为 O(in-progress)，不再深拷贝 committed 集合。

## 3. clog 提交状态与地平线

`src/txn/clog.rs` 的 `CommitStatus` 是按 xid 稠密索引的分段 `AtomicU64` 位图：

- `mark_committed(xid)` 以 `fetch_or` 置位，幂等；`is_committed(0)` 恒 false。
- 读取只取短暂读锁索引一个字，提交者不阻塞读者；位图按 `WORDS_PER_CHUNK` 分块增长。
- `base` 是**真空地平线**：`xid < base` 一律视为已提交，其位图前缀在 `advance_base` 时丢弃。
  地平线只能前移。

地平线的安全前提由 `VACUUM` 保证（见 §9）：低于地平线的 xid 状态已不再被任何存活版本引用，
因而可以安全地冻结为「已提交」。

## 4. 行级锁

`src/txn/lock.rs` 的 `LockManager`：

- 键为 `(table, rid)`，owner 是事务 id；**可重入**（重复加自己已持有的行直接成功）。
- 已被占用则进 FIFO 等待队列；`lock_timeout_ms` 超时报 `lock wait timeout`。
- 等待超过 100ms 时沿 `waiter -> holder` 的等待图做一次死锁检测，成环则回退该等待者
  （`deadlock detected`）。
- `unlock_all(owner)` 释放事务全部锁并把每把锁交给下一个等待者。

接入点在 `src/db/store.rs`：`store_delete_mark` / `store_update_versions` 在改行前
`locks.lock(deleter, name, rid)`，事务提交/回滚时 `unlock_all`。锁**持至事务结束**。

## 5. Read Committed 与 EPQ

默认级别。`Database::execute_stmt_with`（`src/db/mod.rs`）在显式事务内每条语句前刷新
`trx.snapshot = current_snapshot()`，自动提交则在语句开始时取新快照。

写入路径拿到行锁后检查目标版本是否被「本快照之后提交」的事务改动过
（`row_was_concurrently_modified`）：若是，返回内部信号 `Error::Retry`。
`exec::epq_retry`（`src/exec/mod.rs`）用 `rollback_statement` 回滚**本语句**（保留已持有的行锁，
避免被对手抢走导致饥饿），刷新快照后重跑整条 UPDATE/DELETE；最多 `MAX_EPQ_RETRIES = 16`
次，超过报 40001。重跑会重新扫描，谓词不再命中的行自然跳过，不 abort 事务。

## 6. Repeatable Read（40001）

`repeatable_read` 在事务内固定快照。写事务提交时 `check_conflicts`（`src/db/mod.rs`）
对每条 undo 的基版本检查：若它被「快照之后提交」的事务改过（同时检查我们覆盖前的
`prev_deleter` 与页上当前的 deleter），报 PostgreSQL 风格的
`could not serialize access due to concurrent update`（SQLSTATE 40001）。

## 7. Serializable（SSI）

`serializable` = 固定快照 + 简化 SSI（`src/txn/ssi.rs`）：

- **表粒度谓词锁**：事务读某表即登记 reader，写某表即登记 writer；写者为每个读过该表的
  其他事务建立一条 rw-反依赖边 `reader -> writer`。表粒度能捕获幻读，代价是可能否决部分
  本可串行化的调度。
- 只有 serializable 事务参与；与其他级别混用时保证退化为「serializable 之间可串行」（同 PG）。
- 生命周期挂在 `TransactionManager`：`begin_open` → `ssi.begin`，`commit` → `ssi.commit`
  （**保留其边**，直到没有 serializable 事务为止，便于后来者发现经过它的环），
  `remove_open` → `ssi.abort`（清掉触及该事务的边）。
- 提交时 `ssi_conflict(id)` 用 DFS 检测「经过本事务的环」，命中报 40001
  （`serialization_error`）。
- 读钩子：`TableScan`/`IndexScan` 打开、`apply_update`/`apply_delete`、`check_unique`；
  写钩子：`store_insert`/`store_delete_mark`/`store_update_versions`（均经 `Database::note_read`
  /`note_write`）。

## 8. 版本链与唯一约束

- UPDATE：先插新版本，再把旧版本 `delete_mark` 为 `deleter=trx_id, next_rid=pack(new_rid)`；
  undo 记录 `(prev_deleter, prev_next_rid)` 以支持回滚还原（`src/db/store.rs`）。
- 索引项只追加：旧版本项保留给老快照，读时按可见性过滤；stale 项由 vacuum 清理。
- **PK/UNIQUE 在并发下成立**（`src/db/unique.rs`）：`check_unique` 先按索引键经行锁管理器
  加一把合成锁（键名以 `\u{1}` 分隔，避免与真实表名冲突），持至事务结束；然后以**当前已提交
  状态**判定键是否被占用，而不是用快照。并发插同一键时后到者等锁，前者提交后即看到其已提交行
  并报 `duplicate key`。用 `next_rid` 链判断候选是否与 `exclude`（正被更新的行）属同一逻辑行，
  是则跳过，把该冲突交给 EPQ 处理。

## 9. GC / VACUUM 与地平线

`VACUUM`（`src/db/checkpoint.rs::vacuum`，`src/exec/mod.rs::execute_vacuum`）要求无显式/他人
开事务（则 clog 对所有 xid 是终局的）：

- 死行 = `creator` 从未提交（崩溃事务遗留的孤儿版本）**或** `deleter` 已提交。
  回收动作：先删该行在所有索引上的 `(key, rid)` 项，再物理删除堆记录并回收其 LOB。
- 存活但 `deleter` 未提交的行：清掉删除标记（PG 的 un-delete），使低于地平线的 xid 状态可安全
  冻结为已提交。
- 最后 `advance_horizon(next_id)` 并把 `clog_base` 与 `committed_trxs(>= base)` 写入 catalog；
  重开时用 `base + committed` 重建 clog。因此 clog 与 catalog 的 committed 集合都只保留上次
  vacuum 之后的提交，不随历史提交数无界增长。

VACUUM 只删除「即便崩溃重放也不会复活」的数据，故本身不写 WAL；`tests/vacuum.rs` 的
crash-safe 用例锁定该性质。

## 10. 统一 FileId

`src/storage/page.rs` 只定义 `pub type FileId = u32`。表文件的**持久 catalog/WAL 编号与缓冲池
句柄是同一个值**（`catalog::HeapStore { file }`，`src/catalog/mod.rs`）。索引文件与表文件共用
一个缓冲池，因此索引 id 带高位 tag（`INDEX_FILE_TAG = 1 << 31`，`src/db/mod.rs`）避免与表文件
号冲突：低比特仍是落盘用的索引编号（文件名 `<no>.idxf`），`index_file_id`/`index_number`
负责带/去 tag。

## 11. 崩溃恢复（与 MVCC 的衔接）

`src/db/recovery.rs`：只重放带 `Commit` 帧的事务，按提交顺序；重放幂等——
`Record::Insert` 仅在目标槽空时原位写回原 Rid，`Record::DeleteMark` 仅在记录存在且 deleter 为 0
时打标（含 `next_rid` 链的还原）。未提交事务的脏页即便曾被落盘，其行也因 creator 不在 clog 而
不可见，无需 UNDO。被重放触及的表全部重建索引（索引是派生数据，B+ insert 不幂等，先
`discard_file`+`truncate`+`init` 再按堆重灌）。`simulate_crash`（`src/db/mod.rs`）跳过缓冲池
干净落盘来模拟进程被杀。

## 12. 测试

- `src/txn/*.rs` 内联单测：clog 跨块/幂等/地平线、快照 xip、行锁重入/超时/移交/死锁、
  SSI 环与边清理、事务簿记。
- `tests/trx.rs`（22）、`tests/concurrency.rs`、`tests/concurrency_fuzz.rs`（固定种子随机调度，
  断言余额守恒与主键唯一）、`tests/constraints.rs`（三个隔离级各一个并发重复键竞态）、
  `tests/vacuum.rs`、`tests/wal.rs`（18）。
