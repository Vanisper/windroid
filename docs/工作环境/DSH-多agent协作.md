# DSH 多 Agent 协作机制

**本文档性质：环境特异性记录。**

它描述的不是 Windroid 的设计，而是当前 AI 工作环境（DeepSeek Harness）提供的协作能力与实测边界。环境工具会变，本文结论有保质期——每节标注了校验方式，若行为不符，以实测为准并更新本文。

记录动机：这些约束直接决定「谁能做什么」，影响任务如何拆分、交给谁、用什么模型。不记录会导致重复踩坑。

---

## 一、可用的分工单元

| 工具 | 创建者 | 模型可选 | 上下文 | 生命周期 |
|---|---|---|---|---|
| `spawn_teammate` | 仅 Lead | **否**（参数被忽略） | fresh / fork | 持久，可多次收发 |
| `subagent` | Lead | **是**（provider + model + reasoning_effort） | 独立，不带本对话 | 一次性，完成后不再工作 |
| `subagent_fork` | Lead | 是 | 继承当前已完成对话 | 一次性 |

**核对**：`list_agents` 列出所有成员及其 status（running / inactive / provisioning / failed）。注意 inactive 只表示「当前没有回合在执行」，不表示任务完成、成功或失败。

## 二、深度限制：队友不能再派生

这是最影响分工策划的一条。

```
队友调用 subagent        → subagent depth 2 exceeds maxDepth 1
队友调用 spawn_teammate  → only the Team Lead can create teammates
```

**推论**：不存在「团队主模型自己再开子代理」的层级结构。整个协作树只有一层——Lead 派生，派生者不再派生。模型选择权完全集中在 Lead。

因此「让合适的模型做合适的事」只能由 Lead 在派发时决定，无法下放。

## 三、Route 格式

`subagent` 的 `provider` 与 `model` 是两个字段，最终路由为 `<provider>/<model>`。

```ts
// 正确：model 只写 provider 之后的部分
{ provider: "magpie-anthropic", model: "aicentos/claude-opus-5-5" }
//   → magpie-anthropic/aicentos/claude-opus-5-5

// 错误：model 里重复带 provider 会被拼成双重前缀
{ provider: "magpie-anthropic", model: "magpie-anthropic/aicentos/claude-opus-5-5" }
//   → child LLM route "magpie-anthropic/magpie-anthropic/..." is not allowed
```

### 可用 route（实测于本文撰写时）

| provider | model | 说明 |
|---|---|---|
| `magpie` | `deepseek/deepseek-flash` | 快、低成本 |
| `magpie` | `deepseek/deepseek-v4-pro` | |
| `magpie-anthropic` | `aicentos/claude-fable-5-2` | `spawn_teammate` 实际使用的模型 |
| `magpie-anthropic` | `aicentos/claude-opus-5-5` | |
| `magpie-anthropic` | `aicentos/claude-sonnet-5-5` | |
| `magpie-anthropic` | `zcode/GLM-5.3` | |
| `magpie-responses` | `codex/gpt-6.1-sol` | |

推理档位：`low` / `medium` / `high` / `xhigh` / `max`。

**核对方式**：调用 `list_subagent_models`（不带参数列出 provider，带 provider 列出该 provider 下的 model，带 provider + model 列出可用的 reasoning effort）。

## 四、`spawn_teammate` 的模型参数无效

```ts
spawn_teammate({ ..., model: "aicentos/claude-opus-5-5", provider: "magpie-anthropic" })
→ 返回值 member.model 为 "aicentos/claude-fable-5-2"
```

参数被接受（不报错）但不生效。队友固定使用环境默认模型。

**注意**：`list_agents` 返回的 `model` 字段与实际不符（曾出现全部成员都显示同一模型名的情况），不可作为实际模型的证据。唯一可靠的观察点是 `spawn_teammate` 的返回值。

**核对方式**：创建队友时额外传 model 参数，比较返回值中的 member.model 是否与传入值一致。

## 五、消息与等待语义

```
send_message  → 投递到目标的最近步骤边界；目标 inactive 时会启动它
              → 投递成功 ≠ 目标已处理
wait_agent    → 只在调用之后发生的变更才能被观察到
              → 无其他活跃成员时立即返回 noProgress(who: no-active-peer)
              → 从不唤醒 inactive 成员
```

**正确流程**：

```
list_agents → 确认目标是 running / provisioning
  ├─ inactive → 先 send_message 唤醒,再 wait_agent
  └─ running  → 直接 wait_agent
wait_agent 返回或超时 → 重新 list_agents（不要凭上次的快照判断）
```

**等待不等于轮询**：wait_agent 超时后应重新列成员，而不是重复等待同一批可能已结束的成员。

## 六、共享文件系统

所有成员共享同一工作目录与文件系统，编辑立即可见。

**这意味着**：不需要「分发文件」这一步，但也意味着**写冲突是真实风险**。

约定：
- 按不重叠的范围切分写任务
- 在共享任务上记录预期写范围（advisory，不是锁）
- Lead 负责最终审核 diff 并运行测试——队友的写操作不受版本守卫完全保护

**清理责任落在派发者**：子代理可能创建临时目录、启动进程。派发时必须明确要求清理，并在收到结果后自行复核环境状态（进程、设备、临时目录、用户既有数据是否被改动）。

## 七、实际分工建议

基于以上约束，当前环境下的合理用法：

| 场景 | 工具 | 理由 |
|---|---|---|
| 长时间协作、需要多轮往返 | `spawn_teammate` | 会持久存在，可反复 send_message |
| 一次性的深挖调研、实测 | `subagent` + 低成本模型 | 结果导向，不需要保持会话 |
| 基于当前对话上下文的后续工作 | `subagent_fork` | 不需要重新交代背景 |
| 需要精细控制模型的实现任务 | `subagent` + 指定模型 | 是唯一能选模型的途径 |

**成本意识**：调研、查证、批量读取这类任务用低成本模型（如 `deepseek/deepseek-flash`）足够；需要设计判断或复杂重构时才值得用强模型。

**并发限制**：同一时刻活跃成员不宜过多——共享文件系统的写冲突风险随并发上升，且 `wait_agent` 需逐个跟进。

---

## 校验记录

| 结论 | 校验方式 | 最后确认 |
|---|---|---|
| 队友不能派生（maxDepth 1） | 队友调用 subagent / spawn_teammate | 2026-11 |
| 仅 Lead 能创建队友 | 队友调用 spawn_teammate | 2026-11 |
| spawn_teammate 忽略 model | 传入 model 后比对返回值 | 2026-11 |
| route 为 provider + 短 model | 两种写法各调用一次 | 2026-11 |
| list_agents 的 model 字段不可靠 | 与 spawn 返回值比对 | 2026-11 |

环境变更后，按上表逐项复核；与实际不符处以实测结果为准并更新本文。
