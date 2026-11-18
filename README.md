# skflow

**A local workflow and cognitive task CLI for Agent Skills.**

`skflow` 是嵌入 Agent Skill 的本地状态机：普通脚本处理确定性工作，宿主 Agent 或 Subagent 完成认知判断，`skflow` 管进度、发工单、验结果、存状态。

运行中断、跨会话或上下文压缩后，执行 `skflow next` 即可恢复当前工单。无需模型 API、API Key 或独立 Agent Runtime。

## 核心模型

- **State**：SQLite 内的 JSON 状态树。初始输入位于 `/input`，每个步骤只写自己的 `output` 路径。
- **Step**：按 YAML 顺序执行 `run`、`agent`、`check`；`foreach` 可修饰前两种步骤。
- **Task**：持久化的认知工单，包含指令、裁剪后的上下文、输出 Schema 和提交命令。

它不内置业务算法、不调用模型，也不是执行不可信代码的操作系统沙箱。

## 安装

需要 Rust 工具链。当前脚本执行面向提供 `/bin/sh` 的 Unix 主机；本仓库 CI 在 Linux 上验证，示例另需 Python 3。

```bash
cargo install --locked --path .
skflow --help
```

## 五分钟运行示例

仓库提供完整的 [candidate-sourcing 示例](examples/candidate-sourcing/workflow.yaml)：发现候选人 → 去重 → 并发补充事实 → Agent 批量评估 → 质量检查 → HTML 报告。数据均为虚构的本地示例，不进行网络请求。

在仓库根目录执行：

```bash
skflow start examples/candidate-sourcing/workflow.yaml \
  --input examples/candidate-sourcing/input.json
```

命令推进到 `review` 后返回 JSON 和退出码 **10**。三个候选人按每批两个生成两张独立工单。退出码 10 表示等待 Agent，运行没有失败。

读取每张工单的 `instruction`、`context` 和 `output_schema`。`context_file` 与 `schema_file` 提供相同内容的文件形式；使用工单中的提交命令，将答案写入 JSON 文件并提交：

```bash
skflow submit <task_id> --file answer.json
```

`foreach agent` 的答案始终为按实体 Key 索引的对象。以下只展示一个实体的值；实际答案必须覆盖**该工单分配的全部 Key**：

```json
{
  "c-001": {
    "recommendation": "strong",
    "score": 5,
    "reason": "候选人具有 Python 服务与数据管道开发经历，提交的项目材料覆盖目标岗位的核心技能；但生产规模、协作表现及材料真实性仍需在后续面试中核实。",
    "evidence": [
      "候选人材料列出 Python 和数据管道开发经历。",
      "本地补证步骤确认提供了项目材料链接，但没有访问或验证链接内容。"
    ]
  }
}
```

最后一张工单通过校验后，自动执行检查与渲染，返回退出码 **0**。用以下命令查看报告路径：

```bash
skflow inspect /report
```

生成的 HTML 默认位于 `examples/candidate-sourcing/report.html`。

## 工作流定义

```yaml
version: 1
name: candidate-sourcing

steps:
  discover:
    run: python3 scripts/discover.py
    output: /raw_data
    timeout: 60s
    retry: 2

  merge:
    run: python3 scripts/merge.py
    output: /candidates

  enrich:
    foreach: { items: /candidates, key: id, concurrency: 4 }
    run: python3 scripts/enrich_one.py
    output: /enriched

  review:
    foreach: { items: /candidates, key: id, batch: 2 }
    agent:
      instruction: 根据事实和岗位要求评估候选人；说明依据及尚未验证的内容。
      context:
        candidate: "{item}"
        enriched: "/enriched/{key}"
        target: "/input/target"
      schema: schemas/review.schema.json
    output: /reviews

  quality_check:
    check:
      - expr: "length(candidates) == length(keys(reviews))"
        message: 存在未完成评估的候选人
    on_fail: { goto: review, max_visits: 2 }

  render:
    run: python3 scripts/render_report.py
    output: /report
```

工作流中脚本命令的工作目录为 `workflow.yaml` 所在目录，Schema 文件也相对该目录解析。状态路径使用 JSON Pointer，检查表达式使用 JMESPath。

## CLI 与退出码

默认运行数据库为当前目录的 `.skflow/state.db`。可通过全局参数选择独立运行：

```bash
skflow --db /absolute/path/to/run.db start workflow.yaml --input input.json
skflow --db /absolute/path/to/run.db next
```

每个数据库对应一次运行，`start` 不覆盖已有运行。重复执行示例时，可选择新的 `--db` 路径；要重做当前运行的步骤，使用 `rewind`。

| 命令 | 行为 |
| --- | --- |
| `start <workflow.yaml> --input <in.json>` | 创建运行并推进到工单、阻断或完成 |
| `next` | 恢复并幂等推进，重新输出当前待办工单 |
| `submit <task_id> --file <answer.json>` | 校验并提交；步骤完成后自动推进 |
| `status` | 读取当前步骤、进度与待办工单 |
| `inspect <json_pointer>` | 读取指定状态路径；返回对象中包含 `value` |
| `rewind <step_name>` | 使目标步骤及下游结果失效，重新推进 |
| `rewind <step_name> --accept-workflow` | 显式接受工作流修改并重跑；回退点之前必须保持一致 |

除 `--help`、`--version` 外，CLI 输出为 JSON。`status`、`inspect` 同样通过退出码反映当前运行状态；因此成功读取一个等待中的运行也会返回 10。

| 退出码 | `status` | 处理方式 |
| --- | --- | --- |
| 0 | `completed` | 交付最终产物 |
| 10 | `waiting` | 按 `pending_tasks` 完成并提交工单 |
| 11 | `rejected` | 根据 `errors` 修改答案，重新提交 |
| 20 | `blocked` | 检查失败且无剩余回跳预算，报告阻断或人工回退 |
| 1 | `error` | 处理脚本、配置、文件或数据库错误，再恢复 |

在使用 `set -e` 的 Shell 中，应显式处理 10、11、20；不要把所有非零退出码都解释为系统故障。

## 可靠性约定

- **恢复与重跑有区别**：`next` 跳过已落库的成功项；`goto` 和 `rewind` 使目标及其下游结果失效，生成新的工单。旧工单不能覆盖新结果。
- **隔离由引擎执行**：不同步骤的 `output` 路径不得相同或相互包含。Agent 不能指定写入路径；批量答案不能遗漏或增加 Key。
- **提交具有原子性**：整张工单全部验证通过后才写入。重复提交相同 JSON 幂等，不同答案拒绝。
- **并发结果不会相互覆盖**：多个进程可以提交独立工单；命令推进由锁串行协调，脚本项按 `concurrency` 并发执行。
- **脚本允许重新尝试**：成功项逐项持久化；失败项可以恢复。外部脚本与数据库无法组成同一个事务，崩溃窗口内脚本可能再次执行，外部副作用须自行保持幂等。
- **工作流变更须显式接受**：运行使用持久化的定义与 Schema；修改后通过明确的回退操作接纳，避免新旧结果混用。

完整的输入输出、回跳、并发和故障边界见 [语义契约](docs/semantics.md)。

## 接入 Agent Skill

业务 Skill 只需说明如何整理输入、处理退出码和交付产物。可直接采用 [SKILL.md 模板](docs/semantics.md#skillmd-接入模板)。该模板可交给 Claude Code、Cursor 等宿主执行，也可将独立工单分发给原生 Subagent。

## 开发与验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 tests/acceptance.py --binary target/debug/skflow
```

SQLite、JSON Schema 与 JMESPath 提供状态持久化和标准化验证。长度、枚举与必填项约束可以拒绝格式不合格的答案；事实是否真实、判断是否充分，仍由业务 Schema、检查脚本及宿主 Agent 的工作质量共同决定。
