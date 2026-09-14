# chibidb MVCC 与并发控制重构设计（草案 v2）

**v2 变更**：放弃乐观/OCC 方向，**全面以 PostgreSQL 为模板**——MVCC 可见性、锁粒度、
隔离级别（Read Committed / Repeatable Read / Serializable）都按 PG 语义对齐。

本文是**待评审的改进方案**，不是已实现现状。状态：草案，§11 决策已定稿，待进入 Step 1。

---

## 1. 现状诊断（为什么要改）

现在叠了三层并发控制，每层都不彻底：

| 机制 | 位置 | 问题 |
|---|---|---|
| 每库 `RwLock<Database>`，写语句独占整库 | `src/db/instance.rs:33-34, 265-269` | 写粒度是**整库 / 每条语句**，同库写天然串行；本意是读共享，实际把写也锁死了 |
| 可选 2PL `DatabaseWriteLock` | `src/lib.rs:33-72, 112` | 也是**整库一把**，不是行级；与上面那层重叠 |
| FCW 检测 | `src/lib.rs:498-527` | 按 rid 的 `prev_deleter` 检测写写冲突，但因为整库写锁，正常路径几乎不会触发，形同保险丝 |

可见性也不是 PG 式：

- 快照 = **完整 committed 集合的深拷贝**（`src/db/transaction.rs:64`，`src/db/trx.rs:106`），
  每个事务 begin、**每个只读语句**都 clone 一份 → O(n) 时间/内存。
- `committed` 集合只增不减，且**每次写提交都把全量 id 写进 catalog**（`src/lib.rs:481`、
  `src/catalog/meta.rs:61-70`）并 fsync → 每提交 O(n) 磁盘，累计 O(n²)。
- xid 是 `AtomicU32`（`src/db/transaction.rs`），写事务到 2³² 后回卷即破坏正确性。
- 没有：行级锁、等待队列、死锁检测、EPQ、SSI、快照地平线/GC。

结论：可见性层与锁层都没做到位且互相重叠。**目标 = 全面对齐 PostgreSQL 模型。**

---

## 2. 目标与非目标

### 目标
1. **MVCC 可见性按 PG**：每版本 `creator/deleter`（≈ `xmin/xmax`，已具备）+ 快照
   `{xmin, xmax, in_progress}` + xid 状态查询（clog 替身）；快照生成 O(in-progress)。
2. **行级锁按 PG**：写前对元组 `(table, rid)` 加锁；等待队列 + 超时 + 死锁检测/回退。
3. **隔离级别按 PG**：`read_committed`（默认）/ `repeatable_read`(=SI) / `serializable`(=SSI)，
   行为与错误码对齐 PG（冲突报 `40001`，RC 用 EPQ 重读）。
4. **有界状态**：clog/xid 状态带地平线（horizon）与 GC，不随历史提交数无界增长。
5. **xid 64 位**（或明确回卷护栏），消除 2³² 悬崖。
6. 全程**测试绿、分步可合并**，先建后拆，不做大爆炸重写。

### 非目标
- 乐观/OCC（DuckDB 式）——本版不做。
- 分布式事务 / 多节点。
- SSI 的误报优化（先做可用的 SSI，后调）。
- 改变 SQL 语义、索引结构、查询执行器；LSM 引擎并发模型不做大改（只统一可见性接口）。

---

## 3. PostgreSQL 参考模型（本方案的模板）

### 3.1 可见性
- 每个行版本头有 `xmin`（插入它的 xid）、`xmax`（删除/更新它的 xid，0 表示无）。
- 事务快照 `SnapshotData`：
  - `xmin`：所有 `< xmin` 的 xid 都已结束（提交或中止）；`xmax`：快照后新分配的 xid 上界；
  - `xip`：快照生成时仍 in-progress 的 xid 列表；
  - 判定：`xid < snapshot.xmin` 视为已结束（查 clog 定提交/中止）；`xid >= snapshot.xmax`
    必不可见；`xip` 内的必不可见。
- 需要一个 **clog（提交日志）**：查询任意 xid 是 committed / aborted / in-progress。
  PG 用 `xid < xmin` 的地平线 + vacuum 截断 clog；我们对应"committed/aborted 状态 + horizon"。

### 3.2 行锁与 EPQ
- 写（UPDATE/DELETE）前对元组加锁。PG 通常直接以元组头 `xmax` 充当锁位；需要等待时申请
  一次重型事务锁，使等待者睡在持锁 xid 上。
- UPDATE 产生新版本，用 **`t_ctid` 前向指针**把旧版本链到新版本（HOT 链）。
- **Read Committed** 下，等待结束后用 **EPQ（EvalPlanQual）** 取最新已提交版本重评 `WHERE`：
  仍匹配则在其上更新，不匹配则跳过——不报错。
- **Repeatable Read / Serializable** 下，等待结束后若发现目标版本已被"快照之外"的事务改过，
  直接报 `ERROR: could not serialize access due to concurrent update`（SQLSTATE 40001）。

### 3.3 各隔离级别行为（PG 语义，我们照抄）

| 级别 | 快照 | 写冲突时 | 读取 |
|---|---|---|---|
| Read Committed | **每语句**新快照 | 阻塞等前者提交，然后 **EPQ 重读最新版本**，不 abort | 不保证可重复读 |
| Repeatable Read (SI) | **事务首次语句**取一次 | 阻塞等前者提交，然后 **40001 abort** | 整个事务稳定 |
| Serializable | 同 RR 的快照 | 同 RR，且 **SSI** 额外检测 rw-依赖环，命中即 40001 | 可串行化 |

---

## 4. 目标架构

```
             ┌─────────────────────────────────────────────┐
             │  SQL / Exec（不变）                          │
             └───────────────────────┬─────────────────────┘
                                     │
             ┌───────────────────────┴─────────────────────┐
             │  Snapshot / Visibility  (PG 式，核心)        │
             │  · Snapshot { xmin, xmax, xip }             │
             │  · XidStatus (clog 替身：committed/aborted)   │
             │  · visible(xmin, xmax, self)                 │
             └───────────────────────┬─────────────────────┘
                                     │
             ┌───────────────────────┴─────────────────────┐
             │  LockManager（行级元组锁，PG 式）             │
             │  · (table, rid) 锁 + 等待队列 + 超时          │
             │  · 死锁检测/回退；EPQ 重读（RC）              │
             └───────────────────────┬─────────────────────┘
                                     │
             ┌───────────────────────┴─────────────────────┐
             │  VersionStore（creator/deleter + t_ctid 链） │
             │  + GC/Vacuum（horizon 驱动）                  │
             └───────────────────────┬─────────────────────┘
                                     │
             ┌───────────────────────┴─────────────────────┐
             │  Heap / LSM / BufferPool / Disk / WAL（大体不变）│
             └─────────────────────────────────────────────┘
```

关键原则：**只有一套 PG 式 MVCC 可见性 + 一套行级锁**；隔离级别的差异只体现在
"快照何时取"与"冲突后 abort 还是 EPQ 重读"。

### 4.1 关键接口（示意）

```rust
/// PG 式快照：上界 + in-progress 列表，而不是完整 committed 集合。
pub struct Snapshot {
    pub xmax: u32,        // >= xmax 必不可见；< xmax 且不在 xip 的都已在快照前结束
    pub xip: Vec<u32>,    // in-progress xid 列表（升序）
}

/// clog 替身：按 xid 稠密索引的提交位图（`src/db/clog.rs`）。
pub struct CommitStatus { /* 分段 AtomicU64 位图 */ }
impl CommitStatus {
    pub fn mark_committed(&self, xid: u32);
    pub fn is_committed(&self, xid: u32) -> bool;
    pub fn ids(&self) -> Vec<u32>;
}

/// 行级锁管理器。
pub trait LockManager: Send + Sync {
    fn lock_tuple(&self, trx: &TrxCtx, table: &str, rid: Rid) -> Result<()>;
    fn unlock_all(&self, trx: &TrxCtx);
    // 死锁检测/回退、超时由实现内部处理
}
```

---

## 5. 隔离级别与配置

`isolation` 单枚枚举（不再是两轴），**默认 `read_committed`**：

```toml
[transaction]
isolation = "read_committed"    # read_committed | repeatable_read | serializable
lock_timeout_ms = 5000          # 等锁上限（超时兜底）
deadlock_timeout_ms = 1000      # 等多久触发一次死锁检测（PG 的 deadlock_timeout）
```

**作用域**：由配置决定，整个 `Database` 统一；**不做会话级 `SET TRANSACTION`**。
（跨库各用各的配置即可，不涉及同库内级别混用。）

行为完全按 §3.3 的 PG 语义表：RC 用 EPQ 重读不 abort；RR/Serializable 冲突报 40001。

---

## 6. 锁设计（PG 式行锁）

- **粒度**：`(table, rid)` 物理元组锁（同 PG 锁元组，不锁键）。理由：很多表没有 PK、
  更新时键会变、按键锁需走索引且易与 B+ 树结构锁互相死锁。
- **等待**：已被占用则进等待队列（按 `(table, rid)` 的 FIFO / xid 顺序），`lock_timeout_ms` 超时报错。
- **死锁**：`deadlock_timeout_ms` 后触发检测，找到等待环则回滚环中代价最小的事务；超时兜底。
- **释放**：事务结束（commit/rollback）释放其全部锁；支持语句级提前释放的优化后置。
- **EPQ（仅 RC）**：拿到锁后用 `t_ctid` 前向指针找到最新已提交版本，重评 `WHERE` 再应用。
- 替换现有整库 `DatabaseWriteLock`（`src/lib.rs:33-72`）；同时撤掉 Instance 那层
  "写语句独占整库"的隐式串行化（`src/db/instance.rs:265-269`）——这是本方案风险最高的改动，
  放在后面步骤。

---

## 7. 版本存储、EPQ 与回收

- 版本头 `creator/deleter`（`src/storage/codec.rs:33-53`）保留；DELETE=delete_mark，
  UPDATE=mark old + insert new（`src/lib.rs:1115-1155`）保留。
- **新增前向指针 `next_rid`**（旧版本 → 新版本，对应 PG `t_ctid`）：EPQ 沿链找最新版本；
  也让 GC/旧索引清理、"找该行最新版本"更直接。格式改动已获允许（§11 决策 3）。
- **索引**：旧版本索引项保留（供老快照），由 vacuum 清理——保留现有行为。
- **GC / horizon**：`horizon = min(oldest_active_snapshot.xmin, oldest_live_version.xmin/xmax)`；
  低于 horizon 且无存活版本引用的 xid 状态可回收；配合 vacuum（已要求无 open 事务，
  `src/lib.rs:344`）推进。
- **catalog**：不再每提交全量重写 `committed_trxs`；改为 checkpoint / DDL 时保存
  （这直接消除 §1 的 O(n²)）。

---

## 8. 兼容性与格式

- **允许改记录/WAL 格式并升 magic、丢弃旧数据文件**（决策 3）。
  涉及：xid 4→8 字节（codec、WAL 帧 `src/wal.rs:4-5,112-140`、catalog 的
  `committed_trxs`/`next_trx_id`）、快照编码、记录头新增 `next_rid`。
- 一次性动作：`FORMAT_VERSION` / catalog magic 提升；Open 遇旧 magic 明确报错而非误读。
- WAL 视需要增补：`Abort` 帧、xid 宽度。

---

## 9. 分步路线（每步测试绿、可独立合并）

> 原则：**先建后拆**。每步保留现有测试通过，并新增锁定新语义的用例。

- **Step 0｜设计定稿** ✅：本文评审通过（`3c7ff1e`）。
- **Step 1｜去 O(n) 开销** ✅：去掉每提交 `save_catalog`（`4e66b6b`）；
  DML 不再重写 catalog，`catalog.bin` 随历史提交数不再增长。
- **Step 2｜快照与可见性抽象** ✅（`2bdd52e`）：`Snapshot {xmax, xip}` + 稠密提交位图
  `CommitStatus`（clog）；`TrxState` 持 `{ snapshot, clog: Arc<CommitStatus> }`，`visible()`
  改为"`xid < xmax`、不在 `xip`、clog 已提交"；`conflicting_committer`/`vacuum` 走 clog。
  `snapshot()` 从 O(committed) 降到 O(in-progress)。
  *注*：实际 Snapshot 未保留单独的 `xmin` —— 有精确 clog 时 `x < xmin` 与 `[xmin,xmax)`
  的判定合流，`xmin` 只在"用区间近似 clog"（Step 7 的 horizon）时才需要。
- **Step 3｜行级锁管理器** ✅：原语 `LockManager`（`14ee18a`）；**接线完成**——`Database` 持
  `locks: LockManager`，写路径（`store_delete_mark`/`store_update_versions`）在改行前取
  `(table, rid)` 元组锁（owner = trx id），事务结束（commit/全量 rollback）释放全部。
  **`Instance` 不再对 DML 独占整库**：只有 DDL/CHECKPOINT/VACUUM 走 `db.write()`，
  DML 与事务控制走 `db.read()`，同库并发写按行串行。验收：并发自动提交写（4×200），
  以及原来的 FCW 测试改为多线程（第二个写者阻塞→提交冲突）。
- **Step 3.5｜索引并发（B-link tree）**（见 §12）：让 `BTree` 支持并发读写。
  验收：`tests/index_model.rs` 随机模型在并发下通过；`index_btree` 全绿。
- **Step 4｜RC + EPQ**（默认档先做对）：记录头加 `next_rid`；等锁后 EPQ 重读最新版本。
  验收：RC 下「后写者看到前者结果而非 abort」用例。
- **Step 5｜RR / SI**：事务级快照固定；冲突报 40001（不再 FCW）。撤掉 Instance 的整库写独占。
  验收：丢更新回滚、可重复读、并发吞吐随线程上升。
- **Step 6｜Serializable（SSI）**：跟踪 rw-依赖并检测危险结构，命中报 40001。
  验收：写偏斜用例按 Serializable 被拒、按 RR 通过。
- **Step 7｜GC/horizon + xid64**：地平线回收 + 格式升级到 xid64。
  验收：长跑后 xid 状态有界；xid64 单测；崩溃恢复回归。

风险最高的是 Step 5（撤整库写锁），其替代（Step 3–4）必须先到位。

---

## 10. 测试策略

- 复用现有并发/崩溃恢复测试作为**护栏**（`tests/concurrency.rs`、`tests/trx.rs`、
  `tests/wal.rs`、`tests/server.rs`）。
- 三个级别各覆盖：丢更新、可重复读、写偏斜（RR 通过 / Serializable 拒绝）、
  死锁/超时、崩溃恢复后可见性。
- 新增长跑/规模用例：xid 状态与 catalog 大小不随历史提交数线性增长。
- 性能探针：并发吞吐随线程数上升（验证没有被整库锁串行化）。

---

## 11. 决策记录

已定：

1. **放弃乐观/OCC，全面以 PostgreSQL 为模板**（v2）。✅
2. **默认隔离级别 = `read_committed`**（PG 默认）。✅
3. **配置级切换**（`config.toml`），作用域整个 `Database`；不做会话级 `SET`。✅
4. **允许升 magic、丢弃旧数据文件** —— xid64、快照编码、`next_rid` 等无兼容负担。✅
5. **死锁：超时 + 检测/回退都做**。✅
6. **行锁粒度 = 元组 `(table, rid)`；EPQ 用记录头前向指针 `next_rid`（PG `t_ctid`）**。✅

### 11.1 为什么锁元组不锁键
很多表没有 PK、更新时键会变、按键锁需走索引且易与 B+ 树结构锁互相死锁；**PG 本身锁的
也是元组不是键**。

### 11.2 为什么"行了锁"仍可能 abort（RR/SI）
行锁只把"并发写"变成"排队写"，约束的是**何时写**；SI 的冲突判据是"我读到的版本在我快照
之后被别人**提交地**改过"，针对的是**快照**。所以 RR 下等锁后新版本对我不可见，只能 40001；
只有 RC 的语句级快照允许 EPQ 重读最新版本、不 abort。这与 PostgreSQL 的
Repeatable Read 行为一致。

---

## 12. 索引并发（B-link tree）—— Step 3.5

### 12.1 问题
`BTree::insert/delete`（`src/index/btree.rs`）是自顶向下**查找**、再**底向上分裂/合并**：
`insert_rec` 逐层 `read_page` 下降，叶子满则分裂并改叶子、右邻居 `prev/next`、父分隔键；
这些页**逐个** `with_page`（每次只持一页闩），没有 latch coupling/crabbing。两个并发写者
同时分裂/合并会互相覆盖。所以现状 `Database` 的写独占（`db.write()`）是索引正确性的实际保护。

### 12.2 目标：B-link（Lehman-Yao）树
- **节点新增 right-link（右兄弟页号）+ high key（该节点覆盖键的上界）**；叶子已有 `next`，
  可直接作 right-link。
- **查找 lock-fetch**：读一个节点后，若 `key >= high_key` 说明目标在小右邻，沿 right-link
  右移重试；这样读无需持多把锁也能跟上并发分裂。
- **插入 top-down + latch coupling**：下降时持父闩取子闩、随即放父闩（crabbing）；遇到满
  节点就**预分裂**（top-down split），因此不需要在分裂后回拿父闩。
- **删除/合并**是 SMO，最难。两个选项：
  - **D1（建议先做）**：删除/合并用**每索引一把 SMO 闩**（独占）；插入/查找仍用页闩。
    删除较罕见，SMO 期间结构变更互斥即可，插入/查找安全。
  - **D2（完整）**：删除也走 right-link 协议（redistribute/merge），复杂度最高。

### 12.3 格式与底层 API 改动
- `src/index/node.rs`：leaf/internal header 增加 right-link 与 high key（变长，需规划布局）；
  leaf 的 `next` 复用为 right-link。索引文件 **magic/版本升级**（格式可不兼容）。
- latch coupling 需要"**同时持有父闩与子闩**"。两种实现：
  - 复用闭包式 `with_page` 的**嵌套**（父闭包内再 `with_page(child)`）实现 crabbing；
  - 或给 `BufferPool` 增加**页闩守卫** `latch_page(file, no) -> PageGuard`（pin + 页闩的 RAII），
    适合顶向下预分裂需要跨多页持锁的场景。倾向按需再加，避免提前改 `BufferPool`。

### 12.4 子步骤与验收
- **C1a**｜节点格式 ✅（本提交）：leaf/internal 预留 B-link 字段（internal 的 `next` 右兄弟 +
  两类节点的 high-key 区），entry 区起点从 11 移到 `13 + hk_len`；升索引 magic 到 `CHIDBITZ`。
  **high-key 暂不写入（长度恒为 0，无界）故不参与查找**——纯格式步,单线程行为不变。
- **C1b**｜维护 right-link 与 high-key ✅（本提交）：`build_leaf/internal` 写入两者；叶子/内部
  **分裂**设置 `left.next=右半, left.hk=分隔键; right.hk=原 hk, right.next=原 next`；删除的
  **借用/合并**与**根坍缩**同步更新 hk/next（`set_leaf_high_key`/`set_internal_high_key` 重建页）。
  新增公开的 `BTree::check_invariants`（节点内 key < hk、hk = 右兄弟最小 key、最右节点 hk 为空、
  叶链与树一致），随机模型测试每个操作后校验。验收：全绿。
- **C2**｜lock-fetch 下降 ✅（本提交）：`descend` 在 `key >= high_key` 时沿 `next` 右移重试，
  超过 high key 的键归属右兄弟。单线程下行为不变（模型测试全绿），是并发分裂下的安全网。
- **C2.5｜复合键 `(key, rid)`** ✅（本提交）：索引排序/路由键改为 `(key, rid)`，rid 打破平局使
  每条唯一，从而消除非唯一索引的重复串歧义——B-link 无需 `prev` 回溯。分隔键与 high-key 都携带
  rid；`search`/范围扫描用 sentinel rid（`(k,0)` / `(k,MAX)`）映射区间；删除不再需要多候选子节点。
  分裂点选择把 high-key 的 rid 字节计入容量；索引 magic 升到 `CHIDBITY`。不变量测试全绿。
- **C3｜并发插入** ✅（本提交）：插入改**顶向下**——下降时持父闩取子闩（嵌套 `with_page` 实现
  crabbing），进入前对子节点**预分裂**，取消底向上回传分隔键；**根分裂**用页 0 当根锁；文件头记录
  `max_key_len` 作内部节点预分裂预留；入口节点被并发分裂时**从根重试**；并在持闩后**自检本节点是否
  还有空间**，避免"闩外预检"的竞态。索引 magic 升 `CHIDBIV`。并发压力测试（4 线程 × 2000 插入）
  稳定通过，压 60 次无失败/挂起。
  *注*：`prev` 链不再维护（读端已不用它，改为 B-link `next` 链校验）。**删除仍未并发化（C4）**。
- **C4｜并发删除** ✅（本提交）：`delete` 改**顶向下 latch coupling**——持父闩取子闩下降，在叶子里
  删除后，**仍持父闩**地对欠载子节点做 borrow/merge（把 `fix_child`/`fix_leaf_child`/`fix_internal_child`/
  `refresh_separator`/`set_separator_key` 全部改成操作**已持有的父页引用**的 `_at` 变体）。欠载随递归
  **自然向上传播**（每帧持有父闩，检查并修复其子）。并发压力测试（4 线程 × 1500 插入 + 隔一删一）
  稳定通过（压 20 次 0 失败）。`deletes_cause_merge_and_height_shrink` 等单线程测试仍全绿（保留了
  borrow/merge 与高度收缩）。

### 索引并发现状
C1–C4 之后，`BTree` 的**读、插入、删除都支持并发**（lock-fetch + crabbing + B-link），不变量测试与
并发压力测试均通过。数据库层也已**撤掉 DML 的整库写独占**并接线行级锁（Step 3 ✅）。

**剩余**：EPQ（`next_rid` 前向指针）以实现 RC 的"等锁后重读最新版本"；RR/SI 的 40001 语义
（当前 FCW 已近似 SI）；SSI；GC/horizon + xid64。参见 §5–§7 与 §9 路线。
- **C2**｜查找改 lock-fetch（right-link 右移）。验收：并发「读 + 插入」压力下结果与模型一致。
- **C3**｜插入改 top-down + latch coupling + 预分裂。验收：`tests/index_model.rs` 的随机
  模型在**并发**插入/删除下与 `BTreeMap` 模型一致；无损坏。
- **C4**｜删除并发化（D1 或 D2）。验收：并发删除/插入/查找混合压力。

### 12.5 测试
- 现成的 `tests/index_model.rs`（`BTreeMap<Vec<u8>, BTreeSet<Rid>>` 差分模型）是理想 oracle，
  扩展为多线程随机操作并最终与模型对齐。
- 保留 `index_btree`/`index_node` 作为单线程护栏。
- 这一步是**整个方案里并发风险最高**的部分，必须配压力测试，且 C1–C4 分步绿灯推进。
