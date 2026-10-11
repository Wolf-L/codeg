# Codeg：Codex / Claude 原生接入可行性分析

分析日期：2026-10-11。状态：源码与接口调查完成；原生运行时尚未实现。

## 结论与范围

**可行，且不需要逆向 Codex Desktop 或 Claude Desktop 的私有协议。** 推荐保留 Codeg 的界面、数据库、桌面/Web 传输、连接生命周期和事件恢复机制，在运行时边界分别接入：

- Codex：官方 `codex app-server`，使用双向 JSONL RPC。
- Claude：官方 `@anthropic-ai/claude-agent-sdk`，由一个受 Codeg 管理的 Node.js 伴生进程调用。
- 其他 Agent：继续使用原有 ACP 运行时。本次目标是取消 Codex、Claude 两家的 ACP 调用，并非删除整个项目的 ACP 支持。

这是一项中到大型后端改造。界面可以大面积复用，但不能只把启动命令从 `codex-acp` 换成 `codex app-server`，或从 `claude-agent-acp` 换成 `claude`。现有连接握手、审批、能力协商、事件转换和会话控制都包含 ACP 语义。

去掉 ACP 后仍需要把两家的原生事件转换为 Codeg 的界面事件。这层代码服务于 Codeg 自身的数据模型，不需要再次把原生消息转换回 ACP wire protocol。

## 分支与调查基线

| 项目 | 本次状态 |
|---|---|
| 工作仓库 | `Wolf-L/codeg` |
| 新分支 | `native-backends` |
| 上游基线 | `spacering-net/codeg` v0.35.0 |
| 基线提交 | `602590f9c5d557806485f10d193033b4906820a6` |
| 基线中的 Codex ACP | `@agentclientprotocol/codex-acp@2.2.2` |
| 基线中的 Claude ACP | `@agentclientprotocol/claude-agent-acp@0.89.1` |
| Codex 接口核查版本 | `@openai/codex@0.160.1`，与上述适配器声明的 `^0.160.1` 相符 |
| Claude 接口核查版本 | `@anthropic-ai/claude-agent-sdk@0.3.293`，与上述适配器声明的精确依赖相同 |

分支从上游干净提交建立，没有把其他工作目录的未提交修改合入。新分支中的运行代码仍是上游实现，本次新增的是分析及接口清单。

实际完成的验证：读取当前源码；查询两个适配器在 npm 的版本依赖；使用已安装的 Codex 0.160.1 生成协议 JSON Schema；检查已安装的 Claude SDK 0.3.293 公共类型定义，并确认它与该版本公开发布文件的 SHA-256 一致；核验远端分支。未调用模型、未执行原生接入端到端测试、未构建整个应用。因此，本报告确认的是实现路径和改造边界，不是已经达到功能等价。

接口方法、分叉参数、版本和摘要散列记录在 [native-backends-contract-inventory.json](native-backends-contract-inventory.json)。最初发现默认 PATH 中的 Codex 为 0.156.1，随后改用适配器所带的 0.160.1 重新生成并检查；清单采用后者。

## 当前架构与合适的替换位置

当前主要路径：

```text
React UI
  → Codeg Transport（Tauri invoke / HTTP）
  → Codeg commands / web handlers
  → ConnectionManager
  → ACP connection runtime
  → codex-acp / claude-agent-acp
  → Codex App Server / Claude Agent SDK
```

推荐目标：

```text
React UI
  → 现有 Codeg Transport 与会话 API
  → 共享 ConnectionManager / 数据库 / 生命周期
  → RuntimeDriver
       ├─ CodexNative：直接管理 codex app-server
       ├─ ClaudeNative：管理 Node worker → Claude Agent SDK
       └─ AcpRuntime：供其他 Agent 使用
  → Codeg 统一事件、快照、重放
  → 现有 React UI
```

`RuntimeDriver` 是建议新增的内部边界，当前源码中还不存在。三个实现必须共享会话管理、事件序号、审批状态和数据库生命周期，避免出现两套彼此不同步的连接管理器。

| 已核实的源码位置 | 当前职责 | 迁移处理 |
|---|---|---|
| `src-tauri/src/app_state.rs:16,162` | 共享连接管理器、事件总线、数据库与委托注入 | 保留共享核心 |
| `src-tauri/src/acp/manager.rs:680,755` | 连接去重与调用 `spawn_agent_connection` | 在运行时创建位置选择 native 或 ACP |
| `src-tauri/src/acp/connection.rs:1175,1378` | `ConnectionCommand`、`AgentConnection` | 抽出不依赖 ACP wire 类型的内部命令和连接句柄 |
| `src-tauri/src/acp/connection.rs:2397,6323` | 创建 `AcpAgent`、ACP initialize | 两个原生运行时各自实现握手和连接循环 |
| `src-tauri/src/acp/types.rs:53,366` | 事件 envelope 与 Codeg 扩展事件 | 保持兼容，逐步迁到中性命名空间 |
| `src-tauri/src/web/event_bridge.rs:495` | 更新状态、递增序号、缓冲重放、广播 | 原生事件也必须走这条路径 |
| `src-tauri/src/commands/acp.rs:11004` | UI、委托、选项探测共享环境构建 | 复用配置解析，分离适配器专属环境项 |
| `src-tauri/src/web/handlers/acp.rs:79,163` | 安装检查、连接、发送用户消息 | 改为按运行时检查与路由 |

源码依据：[连接管理器](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/acp/manager.rs#L680)、[连接运行时](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/acp/connection.rs#L1175)、[事件桥](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/web/event_bridge.rs#L495)。

### 前端可复用到什么程度

前端 Transport 调用的是 Codeg 后端，不是直接调用 ACP Agent。`src/lib/transport/types.ts:97` 的 `call/subscribe`、`web-transport.ts:161` 的 HTTP 请求、`web-event-stream.ts:177` 的按连接订阅可以保留。

主要复用边界包括：

- `src/lib/api.ts:232`：现有会话控制 API。初期可保留 `acp_*` 作为兼容命令名称；名称保留不等于仍调用 ACP 适配器。
- `src/lib/types.ts:2562,3006,3312`：Codeg 事件、每连接递增 `seq`、`LiveSessionSnapshot`。
- `src/contexts/acp-connections-context.tsx:5924,6093`：快照、重放、实时流恢复和去重。
- `src/lib/adapters/ai-elements-adapter.ts:2402`：统一 `MessageTurn` 到 UI 消息的展示边界。

仍需处理的耦合：Codex 图像生成依靠特定工具标题识别；搜索/命令工具输入由 ACP 标题补齐；子代理父工具 ID、工具调用 ID、MCP 命名有适配器约定。原生运行时必须补齐结构化字段，不能把 SDK 事件原样推给现有 reducer。`claude_sdk_message` 当前主要用于 API retry 展示，并不意味着 Codeg 已有完整 SDK 接入。

设置页需要显示 `runtime_kind`、实际运行时版本及能力；原生 CLI 已安装而 ACP 未安装时，Codex/Claude 应允许启动。ACP 宿主文件/终端工具开关与原生权限设置的含义也必须拆开。

源码依据：[前端事件模型](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src/lib/types.ts#L2562)、[事件恢复](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src/contexts/acp-connections-context.tsx#L5924)、[消息转换](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src/stores/conversation-runtime-store.ts#L747)。

## 原生能力覆盖与需要验证的部分

以下“已存在”仅表示文档或对应版本的协议/类型声明中存在入口，不表示本项目已实现并通过验证。

| 能力 | Codex 0.160.1 | Claude SDK 0.3.293 | 迁移判断 |
|---|---|---|---|
| 新建、恢复、历史 | `thread/start/resume/list/read`、分页 turns/items | `query`、`resume`、`listSessions`、`getSessionMessages` | 基础明确；需要 ID 映射和历史恢复测试 |
| 流式文本与工具 | `turn/start` 与 item/turn 事件 | `query()` 消息流、`includePartialMessages` | 可实现；要规范化工具生命周期和父子关系 |
| 审批和提问 | command/file/permissions approval、`item/tool/requestUserInput` | `canUseTool`、权限模式、hooks | 可实现；不能简化成统一的无条件批准 |
| 停止和追加消息 | `turn/interrupt`、`turn/steer` | `interrupt`、流式输入、`stopTask` | 可实现；两家的排队、追加、停止语义需分别验证 |
| 模型与配置 | `model/list`、thread/turn 参数、provider/config | `supportedModels`、`setModel`、options/env/settings | 可实现；恢复与分叉也要保持配置 |
| MCP | 原生 MCP 配置和相关管理方法 | `mcpServers`、`setMcpServers` | Codeg 自有 MCP 工具应继续可用 |
| 会话 fork | `thread/fork`，有包含式 `lastTurnId` | `forkSession`、`resume + forkSession`、`resumeSessionAt` | 入口明确；界面消息位置不能直接充当原生 ID |
| 编辑旧消息/上下文回退 | `thread/revert` 有 `beforeTurnId`；声明适用于分页线程，且不回退文件 | `resumeSessionAt`、`resumeDropsTurn`；另有独立的 `rewindFiles` | 高风险专项；对话回退与文件回退必须分开 |
| 上下文压缩 | `thread/compact/start` | 压缩状态、边界消息、Pre/PostCompact hooks；手动触发路径仍需实测 | 需完整测试成功、失败、恢复与后续续聊 |
| 子代理、目标和后台任务 | 原生协作事件及 goal 方法 | 父工具 ID、任务事件、`stopTask` 等 | 不能只保留聊天文本；需要独立验收 |

Codex 官方将 App Server 用于深度产品集成，提供会话、审批和事件协议：[官方文档](https://developers.openai.com/codex/app-server/)。公开实现和协议定义位于 [App Server](https://github.com/openai/codex/tree/main/codex-rs/app-server) 与 [app-server-protocol](https://github.com/openai/codex/tree/main/codex-rs/app-server-protocol)。

Claude SDK 提供代理循环、工具、上下文、会话和权限接口：[概览](https://code.claude.com/docs/en/agent-sdk/overview)、[会话](https://code.claude.com/docs/en/agent-sdk/sessions)、[权限](https://code.claude.com/docs/en/agent-sdk/permissions)、[官方 SDK 仓库](https://github.com/anthropics/claude-agent-sdk-typescript)。本次完整 TypeScript 文档页面抓取失败，因此具体方法/参数以本机对应版本的发布包 `sdk.d.ts` 核对，不能把在线文档的新能力无条件套到旧版本。

### Codex 实现路线

由 Rust 后端启动并管理 `codex app-server`，实现请求 ID 对应、通知消费、服务器反向请求、连接关闭和进程退出。使用 stdio 起步，避免初期增加额外网络监听和 WebSocket 兼容面。

对每个 workspace/provider/环境组合明确进程隔离策略，不宜一开始把不同身份或不同配置的全部会话塞进一个全局进程。复用现有进程树清理、stderr 截断、cwd 和 Windows 启动处理；协议编解码不能复用 ACP 的握手。

审批需要保留原生选择和上下文，例如只批准一次、规则变更、网络权限、拒绝和取消，不能只传一个布尔值。原生 thread/turn/item ID 应成为来源身份，同时保持 Codeg conversation/connection ID。

### Claude 实现路线

推荐一个小型 Node.js worker，依赖官方 Agent SDK；Rust 通过内部结构化 stdio 消息控制它。worker 负责 SDK 的异步输入输出、`canUseTool` Promise、hooks、会话句柄和关闭，Rust 继续负责 Codeg 数据库、事件排序及客户端连接。

采用 Node worker 并不意味着重新引入 ACP：它只是 Rust 与官方 JS SDK 之间必要的进程边界。直接照抄 Claude Desktop 私有通信没有必要；从 Rust 手写 SDK 背后的全部控制协议会增加兼容维护成本。

必须显式定义 `cwd`、`env`、`settingSources`、系统提示词策略、MCP 和权限模式。SDK 默认行为与“启动用户交互式 CLI”不能自动视为相同。需要双向交互和运行中控制时采用流式输入，正确区分单轮 result、队列中仍有用户消息、后台子任务完成以及整个 session 关闭。

## 不能遗漏的行为

### 审批、恢复与事件一致性

现有审批请求包含队列、撤销和取消状态；Codeg 的权限 responder 当前仍与 ACP responder 绑定，需抽成运行时无关接口。Claude 的 `canUseTool` 不会为已经提前批准的每次调用触发；`allowedTools` 主要是自动批准规则，不能当成工具集合的完整过滤器。需要逐次拦截时评估 `PreToolUse`。这是行为映射问题，不能通过开启 bypass 省略。

原生输出也必须经过现有事件桥，保留 `seq`、snapshot/replay/live 的顺序和去重。否则浏览器刷新、多窗口查看、权限等待时重连、断线后工具卡片恢复都会退化。须保留并发连接去重以及写数据库之前的 turn admission 锁。

### 历史、分叉和编辑

当前基线普通 fork 在目标无法定位时可能退化为尾部分叉；源码中没有 `acp_edit_fork` 的严格历史编辑契约。因此不能把“当前 fork 能运行”作为严格编辑功能已具备的证据。

原生迁移应保存 provider thread/session、turn/message/item 的真实身份，并在裁剪之前验证选定的上下文前缀。Codex 的 `lastTurnId` 是包含式边界；Claude 的 `resumeSessionAt` 也包含指定链条项。第一次用户消息、工具结束的回合、排队消息、压缩之后的历史、晚到的子任务结果都需要专门用例。

`thread/revert` 的声明限定分页线程，不能未经测试把它当成任何旧线程都可用的通用回退。若产品要求保留原分支，须验证分叉后再裁剪的组合，而不是原地改掉原线程。Claude 的 `resumeDropsTurn` 可提供额外校验，但属于具体版本和运行路径的能力；拒绝时应保留原会话与用户草稿，不能静默尾部分叉或循环重试。

会话分叉和文件回退是两个操作。Claude `rewindFiles` 依赖文件检查点；Codex 此处 `thread/revert` 的描述明确只修改持久化对话历史。普通“编辑旧消息”不能隐式回滚工作区文件。

还要区别稳定的 Codeg conversation ID、活动 runtime session handle 与磁盘 transcript ID。当前 Claude `/clear` 已存在 transcript rollover 处理，迁移时不可只存一个万能 session ID。

源码依据：[fork 目标解析](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/acp/manager.rs#L1894)、[生命周期](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/acp/lifecycle.rs#L358)。

### 自有工具、后台任务和安装

Codeg 的浏览器、电脑、Office、委托等自有工具不能随 ACP 连接一起丢失。应保留当前工具服务，通过原生 MCP 配置注入；区分由引擎执行的工具和由 Codeg 宿主执行的操作，后者不会自动受引擎沙箱约束。

前台会话、委托子会话、待办/自动化任务、聊天渠道和选项探测都必须经过同一个 runtime selector。只迁移前台会得到“UI 看起来原生、后台仍调用 ACP”的不完整结果。安装诊断和升级也要从适配器版本转为原生 CLI/SDK 版本，并验证 Windows、macOS、Linux、服务器/Docker 所需的 Node 与 CLI 分发。

MCP、Skills、项目配置、provider 环境覆盖、断线后的后台运行和停止整个进程树都应进入验收。移除 ACP 不会自动解决引擎与模型网关之间的协议兼容问题，例如压缩请求或上下文加密状态不兼容。

第二轮源码审计进一步确认以下非前台路径，必须纳入改造：

| 具体位置 | 当前行为 | 原生迁移要求 |
|---|---|---|
| `acp/delegation/spawner.rs:85`、`acp/manager.rs:4033` | 委托通过 `ConnectionSpawner` 直接调用 manager，绕过 UI connect command | 在 manager 下统一选择 runtime；保留父子 conversation、tool ID、任务关联和复用标记 |
| `acp/connection.rs:5210,5681` | 用户 MCP 配置直接转 ACP 类型；自有 MCP 注入绑定父连接、cwd、一次启动令牌和进程 | 先抽出中性 MCP launch config，再转换为两家原生配置；保留撤销与孤儿清理 |
| `work_task/compact.rs:30`、`work_task/engine.rs:2158` | Claude/Codex 的 `/compact` 被作为普通 linked prompt 发送，并有完成事件隔离 | 增加有类型的 compact 操作或经过验证的命令映射；不能把压缩完成当成任务完成 |
| `automation/engine.rs:453,738` | 自动化无 UI 地启动 manager，收到 `TurnComplete` 后结束运行并断开 | item 完成、压缩完成、子任务完成不可误映射为整个自动化完成 |
| `chat_channel/session_commands.rs:1522` | 渠道按 external ID 直接恢复连接 | 渠道也需要原生恢复、排队、审批和取消验证 |
| `db/entities/conversation.rs:52`、`parsers/mod.rs:376` | 持久化 agent type 与一个 external ID；历史解析按 agent type 选择 | 增加后端/历史格式版本或明确迁移映射；避免全局切换后误解释旧 ID |
| `commands/conversations.rs:1364` | 找不到对应 transcript 时可返回空 turns | 必须验证 SDK 日志位置和原生 ID；实时能聊天不等于重启后历史可读 |
| `parsers/mod.rs:101` | 备份依赖 Claude projects、Codex sessions 及标题索引路径 | 保留旧备份的相对路径解释；新存储位置须显式接入备份/恢复 |
| `update/install.rs:365` | server 自更新只验证并替换预定的二进制和 web 文件 | Node worker 仅放入发布压缩包还不够，必须进入自更新与回滚清单 |

其中发现一个需要重点回归的既有细节：`chat_channel/session_event_subscriber.rs:533` 的自动审批寻找 `allow` / `allowForSession`，找不到时取第一个选项；而当前 ACP 转换使用 `allow_once` / `allow_always`（`acp/connection.rs:8224`）。新运行时改变选项顺序可能改变最终选择。迁移应统一审批语义并测试“拒绝选项排在第一位”的情况，不能把现有顺序当作可靠契约。本次仅记录，没有修改该行为。

`codeg-mcp` 是 Codeg 自有工具的 MCP 伴生程序，删除 Codex/Claude 的 ACP 适配器不应删除它。Docker 当前提供 Node，但桌面/独立服务器不能据此假定已经安装 Node；两种部署的 worker 分发和依赖检查都需要完成。

源码依据：[委托抽象](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/acp/delegation/spawner.rs#L85)、[任务压缩](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/work_task/engine.rs#L2158)、[自动化结束](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/automation/engine.rs#L738)、[渠道审批](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/chat_channel/session_event_subscriber.rs#L533)、[服务器自更新](https://github.com/spacering-net/codeg/blob/602590f9c5d557806485f10d193033b4906820a6/src-tauri/src/update/install.rs#L365)。

## 方案比较

| 方案 | 优点 | 代价/问题 | 判断 |
|---|---|---|---|
| 共享管理器下增加原生 runtime drivers | 保留 UI、生命周期和持久化；两个引擎真正绕过 ACP | 需要抽出命令/事件/审批边界 | **推荐** |
| 原生调用后再包装为内部 ACP server | 初期能复用更多 ACP 客户端代码 | 把第三方适配器换成自己维护的 ACP 适配器，仍受同类协议映射约束 | 不符合此次主要目标 |
| 新写整个 WebUI 和后端 | 初期结构可以较简单 | 需要重做现有工作区、任务、历史、审批、恢复、跨端等产品能力 | 成本与回归面过大 |

公共模型中仍叫 `AcpEvent`、HTTP 路由仍叫 `acp_*` 可以作为短期兼容安排，但需明确它们是 Codeg 自有 DTO/API。不能让新 Codex/Claude runtime 继续依赖 ACP `SessionUpdate`、ACP handshake 或 ACP permission responder，否则只是换了外壳。

## 建议实施顺序与验收门槛

1. **抽出运行时边界。** 定义 `runtime_kind`、共享连接句柄、命令、能力和审批 responder；确保既有 ACP Agent 行为保持可用。Codex/Claude 选择 native 后，缺少原生依赖应明确报错，不能自动退回 ACP。
2. **先贯通 Codex。** 启动/握手、新建/恢复、流式输出、命令/文件审批、取消、关闭和 UI 恢复；使用与锁定版本匹配的生成类型或 Schema。
3. **再贯通 Claude。** Node SDK worker、流式输入、工具审批、模型配置、会话恢复、关闭、MCP 注入；明确结果事件与整个会话生命周期的区别。
4. **补齐高级功能与所有后台入口。** 历史/分叉/严格编辑、压缩、子代理、委托、待办、自动化、渠道、浏览器/电脑/Office 工具。
5. **切换安装和发布。** 移除这两家的 ACP 下载/启动/探测路径，保留其他 Agent 的 ACP；锁定原生依赖，完成桌面和服务器构建，再整理公共 `Acp*` 命名。

工作量分级：步骤 1 是架构重构；步骤 2、3 是两个独立运行时的主要实现；步骤 4 是功能等价的最大不确定项；步骤 5 是多平台交付。基础聊天连通的原型可以较小，但不能把它的工期当成完整替代的工期。在未完成步骤 1 的接口拆分和真实原生 smoke test 前，不给可靠的日历工期或“100% 功能覆盖”承诺。

最低验收集合：

- Codex/Claude 的前台、委托、后台任务和选项探测均不启动 ACP 适配器，也不发送 ACP initialize/session 请求。
- 新建、恢复、重连、两个窗口同时查看、重复连接、busy 拒绝，不产生重复会话或重复消息。
- 文本/思考、工具部分更新、文件差异、图片、子代理父子关系、重试与 token 统计正确展示。
- 审批允许/拒绝/取消/撤销、等待审批时刷新、退出后清理、策略覆盖均符合用户设置。
- 停止正在运行的回合、取消排队消息、终止子任务的语义分别验证，不把几种操作合成同一个盲目 kill。
- 从 CLI 原生日志导入后续聊；分叉的模型/provider/cwd 不丢失；原分支保留。
- 严格历史编辑无法定位时明确失败；第一轮、工具结束回合、排队消息、压缩后历史、晚到子任务覆盖；不会隐式改动工作区文件。
- 压缩成功/失败后状态正确；成功后能继续工作；失败不被记成完成。
- MCP/Skills、Codeg 浏览器/电脑/Office/委托工具可用；宿主工具权限不会因迁移扩大。
- Windows/macOS/Linux 桌面、服务器/Docker 的启动、退出、依赖检查、升级和进程树清理通过。

先做离线的原生事件样本回放和权限状态机测试，再在临时工作区做两家的真实端到端验证。现有前端快照、断线恢复、工具映射和权限测试可复用。源码未改动的本次分析不运行整套构建或模型测试。

## 本次仍未证明的内容

- 全部 Desktop 产品功能都能由 SDK/App Server 直接获得；公开入口主要覆盖代理运行时，产品服务仍需逐项分析。
- 原生接入一定更快、更省 token、更聪明或更稳定。去掉一层转换是结构上的变化，收益必须通过实际测量和回归验证。
- Claude 手动压缩及高级追加/队列取消在目标 SDK 组合下与当前 Codeg 行为完全一致。
- 所有旧格式/压缩后的 Codex 会话都能采用同一种严格历史裁剪方式。
- 任意 provider 网关、订阅认证或用户环境在原生路径上自动兼容；实施时应针对实际部署方式验证，而非依赖 Desktop 私有凭据。

最终建议：推进此分支，采用“保留 Codeg 产品层、替换两家运行时”的路线。先以一条完整的“原生启动 → 工具审批 → 文件修改 → 停止/恢复 → 再次对话”链路作为试验门槛，再做高级能力和全入口迁移。
