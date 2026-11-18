# skflow 运行语义

本文说明 v1 MVP 的可观察行为。`State`、`Step`、`Task` 是业务作者需要理解的三个概念；轮次、锁和事务属于引擎实现细节。

## 状态与路径

运行创建时，输入 JSON 存入状态树 `/input`。步骤的 `output` 声明它唯一可写的子树，例如 `/candidates` 或 `/reviews`。元数据、任务状态和运行游标存放在 SQLite 中，不属于业务状态树。

路径使用 JSON Pointer：以 `/` 开头，`~0` 表示 `~`，`~1` 表示 `/`。检查表达式使用 JMESPath，不能把 JSON Pointer 直接作为表达式使用。

输出路径不能是根路径，不能覆盖 `/input`，也不能与另一步骤输出相同或互为祖先。例如 `/facts` 与 `/facts/details` 冲突，`/fact` 与 `/facts` 不冲突。引擎拒绝会覆盖已有非对象父节点的写入。

对于 `foreach`，结果位于 `output/<key>`。实体 ID 是对象键，包含 `/` 或 `~` 也不会成为额外路径层级。所有 Key 在执行前解析并去重；仅接受非空字符串或整数，且转成存储字符串后也必须唯一。

这些约束保护通过引擎提交的结果。`run` 执行的脚本是可信本地程序，可以使用进程原有的文件与网络权限；`skflow` 不提供操作系统沙箱，不试图防止脚本直接篡改数据库。

## `run` 的输入输出

脚本命令在工作流文件所在目录执行。stdin 传入 JSON；stdout 必须为一个完整 JSON 值。日志写入 stderr，不要混入 stdout。

### 普通步骤

默认 stdin 为执行该步骤时的完整状态树：

```json
{
  "input": { "target": "Python 工程师" },
  "candidates": [{ "id": "c-001", "name": "林舟" }]
}
```

stdout 的 JSON 值整体写入步骤的 `output`。

### 单项 foreach

`batch` 默认为 1，stdin 包含：

```json
{
  "state": { "input": { "target": "Python 工程师" } },
  "item": { "id": "c-001", "name": "林舟" },
  "key": "c-001"
}
```

stdout 是当前项的裸结果。引擎选择输出键，脚本无需再包一层 `c-001`。

### 批量 foreach

配置 `batch: N` 且 `N > 1` 时，stdin 包含：

```json
{
  "state": { "input": {} },
  "items": [
    { "key": "c-001", "item": { "id": "c-001" } },
    { "key": "c-002", "item": { "id": "c-002" } }
  ]
}
```

stdout 为精确覆盖本批 Key 的对象：

```json
{
  "c-001": { "has_material": true },
  "c-002": { "has_material": false }
}
```

最后一批即使只有一项，也沿用配置为批量时的协议。

可选 `input` 投影用于缩小脚本可见的状态；单项和批量 foreach 的 `state` 字段替换为投影后的值，项与 Key 的封装保持不变。

### 超时、重试与恢复

`timeout` 限制一次脚本尝试的时间，默认 300 秒；`retry: 2` 表示初次尝试之外最多再尝试两次。超时或脚本失败不应把不完整 stdout 写入状态。

超时同时覆盖 stdin 写入、进程执行与输出管道排空。单次 stdout 上限 16 MiB、stderr 上限 64 KiB，超过上限会报错。正常退出、失败与超时会清理该脚本的进程组；若 CLI 本身被 `SIGKILL` 等方式强制终止，清理逻辑无法运行，旧脚本可能仍在运行，宿主应在重试前确认其外部副作用可安全重试。

单项成功即持久化；并发执行时，另一个项失败不撤销已成功项。之后执行 `next` 只补未完成项。批量执行以一批为提交单位，失败批次会重新执行。

脚本与 SQLite 不共享事务。如果脚本已产生外部副作用，但进程在结果落库前崩溃，恢复时可能再执行该脚本。需要避免重复发送、重复扣款等副作用的业务脚本，应自行用稳定标识实现幂等；引擎不承诺外部副作用 exactly-once。

## `agent` 的工单和答案

普通 agent 的 `context` 为投影结果，答案直接符合声明的 Schema。未声明 context 时使用空对象。

foreach agent 按 `batch` 分配独立工单，context 为：

```json
{
  "items": [
    {
      "key": "c-001",
      "context": {
        "candidate": { "id": "c-001" },
        "enriched": { "has_material": true }
      }
    }
  ]
}
```

`"{item}"` 表示完整当前项，`"{key}"` 表示当前 Key，`"/enriched/{key}"` 从状态树取得对应项。引用 Key 时按路径段转义。context 对象可以组合所需字段，避免给宿主 Agent 传入整棵状态树。

foreach agent 的 Schema 描述**一个实体的结果**；引擎导出的 `output_schema` 描述该工单的完整答案封装。答案始终为 Key 对象，包括 batch=1 的情况：

```json
{
  "c-001": {
    "recommendation": "possible",
    "reason": "这里填写足够完整、符合业务 Schema 的判断依据。"
  }
}
```

答案 Key 集合必须恰好等于工单分配集合。多出或遗漏任意 Key、任意单项不满足 Schema，整张工单返回 11，状态不变化。所有项通过后才原子落库。

Schema 支持标准 JSON Schema 约束与内部引用，不加载外部引用。工单使用其创建时固定的 Schema，之后修改磁盘文件不会悄悄改变已发工单的校验标准。

工单同时提供内联 `context`、`output_schema` 和导出的 `context_file`、`schema_file`。数据库保存恢复工单所需的内容，删除导出文件后可用 `next` 重新生成。`status` 和 `inspect` 只读取快照，不创建导出文件；即使文件丢失，内联内容仍然完整。使用工单提供的提交命令，可以避免 Subagent 的当前目录不同而选错运行。

### 重复提交

- 同一有效工单重复提交相同 JSON，幂等接受，不重复写入或执行后续步骤。
- 向已完成工单提交不同 JSON，返回 11。
- 提交已被回退失效的旧工单，返回 11。
- 无效 JSON、越界 Key、Schema 失败，返回 11；文件读取或数据库异常返回 1。

JSON 对象键的排列顺序不影响“相同 JSON”的判断。

## 并发

宿主可将不同 `task_id` 分给多个 Subagent，各自执行提交命令。引擎协调同一运行的状态修改与自动推进；多个 CLI 进程可以同时调用，实际修改串行提交，不会发生整树覆盖或重复推进。只读状态查询不启动业务脚本。

有命令正在执行长时间脚本时，`status` 读取最近已提交的进度快照，运行状态为 `waiting`，步骤进度可显示 `running`，但 `pending_tasks` 可能为空。此时已有执行进程持有推进权，不能把空工单列表当作整个流程完成；`next` 会等待推进锁后继续检查。退出码 0 才表示运行完成。

`foreach.concurrency` 决定脚本项的并发上限；Agent 工单由宿主调度，`skflow` 不创建模型会话或管理模型并发。批次中的上下文和任务分配在发单时固定。

## check、恢复与回退

每条 check 的 `expr` 是 JMESPath 表达式，对当前业务状态树求值。断言不通过时返回其 `message`，没有可用回跳则进入 `blocked`。

`on_fail.goto` 指向较早的步骤；`max_visits` 是目标步骤的**总访问次数上限，包含首次执行**。例如 `max_visits: 2` 允许首次执行加一次重跑。计数持久化，反复执行 `next` 不会增加预算。

| 操作 | 已成功项 | 目标与下游输出 | 旧工单 |
| --- | --- | --- | --- |
| `next` | 保留，继续未完成项 | 保留有效结果 | 当前待办仍有效 |
| check 回跳 | 回跳范围重新执行 | 清除回跳目标及下游输出 | 回跳范围工单失效 |
| `rewind <step>` | 回退范围重新执行 | 清除目标及下游输出 | 回退范围工单失效 |

回跳与回退创建新的内部执行轮次，因此较慢的旧 Subagent 无法覆盖新结果。显式人工回退重置下游访问计数；自动 check 回跳保留相应预算，防止无限循环。

`rewind` 会立即重新推进，可能执行脚本或发出新工单。先通过 `status`／`inspect` 确认回退点，可以减少不必要的业务重跑。

## 工作流一致性

运行记录工作流定义与 Schema 快照及其摘要。摘要覆盖 YAML 与所引用 Schema 文件的内容，包括仅修改注释或格式的情况。发现不一致时，推进命令返回错误，说明需要显式接受修改；查询命令仍可用于检查状态。

```bash
skflow rewind review --accept-workflow
```

上述操作只有在 `review` 之前的步骤及其 Schema 保持一致时才允许。若修改了更早步骤，应回退到最早修改点，或在新数据库创建新运行。引擎同时清除旧版和新版下游输出路径，避免路径变更留下过期字段。

回退不能越过尚未完成的前序步骤。它用于重新执行已到达的步骤，不是跳过执行顺序的入口。

脚本文件不是执行沙箱，也不是不可变部署包。更改脚本后应明确回退受影响步骤；不要把“工作流摘要未变”理解为所有外部依赖都未变。

## SKILL.md 接入模板

以下为复制到业务 Skill 的模板内容，仓库不安装或注册任何个人 Skill。

````markdown
---
name: your-business-skill
description: 复杂业务技能描述
---

## 执行规约

本技能由 skflow 管理状态与步骤流转。请按返回的工单操作，不自行跳过步骤或修改状态库。

1. 将用户需求整理为 input.json，选择本次运行独立的数据库路径，执行：
   `skflow --db /absolute/run/state.db start workflow.yaml --input input.json`
2. 解析 JSON 输出并处理退出码：
   - 10 / waiting：读取 pending_tasks 中的指令、上下文和 Schema，完成答案文件后使用该工单的 submit 命令提交；独立工单可并发交给 Subagent。
   - 11 / rejected：按照 errors 修正同一任务的答案，再次 submit。
   - 0 / completed：读取 /report，向用户交付最终产物。
   - 20 / blocked：说明具体断言与阻断原因，由用户或宿主判断是否 rewind。
   - 1 / error：说明并排查配置、脚本或文件错误；修复后执行 next 恢复。
3. 上下文压缩、中断或不确定进度时，执行：
   `skflow --db /absolute/run/state.db next`
4. 不通过手工编辑 SQLite 或修改答案 Key 绕过校验。重新执行已完成步骤应使用 rewind。
````
