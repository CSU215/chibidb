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
/// PG 式快照：区间 + 例外列表，而不是完整 committed 集合。
pub struct Snapshot {
    pub xmin: u32,               // < xmin 都已结束
    pub xmax: u32,               // >= xmax 必不可见
    pub xip: Box<[u32]>,         // in-progress xid 列表
    pub self_xid: u32,
}

/// clog 替身：任意 xid 的最终状态。
pub trait XidStatus {
    fn is_committed(&self, xid: u32) -> bool;
    fn is_aborted(&self, xid: u32) -> bool; // 两者皆假 = in progress
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

- **Step 0｜设计定稿**：本文评审通过。验收：文档合入。
- **Step 1｜快照与 O(n) 开销**：`Snapshot {xmin,xmax,xip}`；`snapshot()` O(in-progress)；
  去掉每提交 `save_catalog`（改 checkpoint/DDL 保存）。
  验收：新增「连续 N 次提交后 catalog 大小/提交延迟不随 N 增长」测试；回归全绿。
- **Step 2｜可见性抽象**：`TrxState.snapshot: HashSet` → `Snapshot` + `XidStatus`；
  `visible()` 改 PG 区间判定。验收：现有一致性/隔离测试全绿 + 可见性边界单测。
- **Step 3｜行级锁管理器**：原语化 `LockManager`（tuple 锁 + 等待队列 + 超时 + 死锁检测），
  替换整库 `DatabaseWriteLock`。验收：不同行不阻塞 / 同行按序等待 / 超时 / 死锁回退用例。
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
