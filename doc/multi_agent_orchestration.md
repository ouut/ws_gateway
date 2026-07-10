# 多 Agent 协同实现 Rust 网关 —— 编排方案与指令模板

配合 `rust_gateway_prd_v3.md` 使用。适用于 Claude Code 的 subagent/Task 机制，也适用于多个独立对话窗口手动协同。

---

## 0. 总体流程（闭环）

```
1. Coordinator 拆任务 + 分发协议契约
2. 各 Worker Agent 并行实现自己的模块（不互相依赖代码，只依赖第4章协议）
3. Coordinator 做一次集成（cargo build 打通）
4. Review/QA Agent 对照 PRD 第9/11章跑验收
5. 不通过 → 打回给对应 Worker Agent，带上具体失败项 → 回到步骤2
6. 全部通过 → 交付
```

不要让 Worker Agent 之间互相通信协调接口——接口在 PRD 第 4 章已经锁死，谁改协议谁通知 Coordinator，Coordinator 统一广播修订，避免"电话游戏"式的信息失真。

---

## 1. Coordinator Agent 指令

```
你是本项目的协调者（Coordinator）。你手上有完整的 PRD（rust_gateway_prd_v3.md）。

你的职责：
1. 阅读 PRD，将其拆解为以下 5 个独立任务包，每个任务包只包含该 Agent
   需要的 PRD 章节摘录（不要整份丢给下游），另附一份「第4章协议契约」
   作为所有任务包的公共附件：
   - Task A: 路由核心与并发数据结构（对应 PRD 第 3.3/3.4/5 章）
   - Task B: 网络 I/O 层 —— UDP 接收器 + WS 读写分离（对应第 2/3.1/3.2 章）
   - Task C: 可观测性与运维（对应第 8 章）
   - Task D: 客户端 SDK 示例（对应第 6 章）
   - Task E: 静态文件服务（对应第 7 章）

2. 每个任务包必须包含：目标、输入接口、输出接口、边界情况清单（从第9章
   筛选出与该模块相关的条目）、以及"不要做的事"（避免模块间职责重叠）。

3. 所有下游 Agent 完成后，你负责把代码集成到同一个 cargo workspace，
   跑一次 `cargo build`，解决模块间的类型/接口不一致问题。

4. 集成通过后，把完整代码库交给 Review/QA Agent。

5. 如果 Review/QA Agent 打回问题，你要判断问题属于哪个 Task，把具体的
   失败项（不是笼统的"有bug"，而是"第9章第X条边界情况未处理，测试用例：xxx"）
   转发给对应的 Worker Agent，并要求其只修改这一处，不做无关重构。

不要自己写业务代码，你只做拆解、分发、集成、转发。
```

---

## 2. Task A：路由核心 Agent 指令

```
你负责实现网关的路由核心模块。只读以下内容，不需要了解 UDP/WS 网络层
的实现细节，只需要知道它们会调用你暴露的接口。

【输入】第4章协议头定义（22字节头部字段表）
【依赖】DashMap<RoomID, DashMap<UserID, mpsc::Sender<Bytes>>>

你要实现：
1. 连接注册/注销接口：register(room, user, sender) -> Result；
   unregister(room, user)。
2. 路由分发接口：route(header, payload: Bytes)，根据 Target Type
   做 BROADCAST（房间内全量分发）或 UNICAST（精准单发）。
3. 房间自动销毁：unregister 后检查房间是否为空，为空则从外层 DashMap 移除。
4. 背压策略（PRD 5.3）：
   - RAW_MOTION 类型：channel 满时丢最老帧、塞最新帧（用 try_send +
     失败后 try_recv 清一个再 try_send 的方式实现，不要用 unbounded）。
   - SYSTEM_CMD 类型：满了先重试 <=50ms，仍失败则触发断连回调。
5. 暴露给 Task C 的 metrics 埋点接口（不要自己实现 metrics 导出，
   只需要在路由成功/丢弃/延迟处调用注入的回调）。

明确不要做的事：
- 不要解析 Payload 内容。
- 不要做鉴权（PRD 已声明不做，第10章）。
- 不要做 CRC 校验（PRD 已声明不做）。
- 不要处理 socket accept/listen，那是 Task B 的职责，你只处理已经
  拿到 Bytes 之后的路由逻辑。

交付：一个可独立单元测试的 Rust module，附带覆盖 PRD 第9章中路由相关
边界情况（房间不存在、用户不存在、重复连接踢出、队列满）的单元测试。
```

---

## 3. Task B：网络 I/O Agent 指令

```
你负责实现网关的网络接入层：UDP 接收器 + WebSocket 读写分离 + 心跳。
使用 axum + tokio。

你要实现：
1. UDP 监听 :9999，独立协程死循环 recv_from，收到包后立即校验：
   - 总长度 >= 24 字节，否则丢弃
   - Version 字段匹配当前版本，否则丢弃
   - Length 字段与实际 payload 长度一致，否则丢弃
   校验通过后转成 bytes::Bytes，tokio::spawn 交给 Task A 的
   route() 接口，接收循环本身不能有任何 await 阻塞点。

2. WS 监听 :8080/ws，从 query 里取 room/user（无需 token，PRD 已声明
   不做鉴权），为每个连接建立独立的 Read Task 和 Write Task：
   - Read Task：收到二进制帧后做与 UDP 相同的头部校验，调用 route()。
   - Write Task：从 Task A 分配的 mpsc::Receiver 里取数据写回客户端。
   - 心跳：每 10s 发 Ping，30s 未收到 Pong 判定断线，调用 Task A 的
     unregister()。

3. 重复连接处理：register 前先检查 room+user 是否已存在，若存在则
   先踢掉旧连接（关闭其 WS）再注册新连接。

明确不要做的事：
- 不要做 token/鉴权校验（PRD 第10章已声明不做）。
- 不要做 CRC 校验（PRD 已声明不做）。
- 不要自己维护路由表，所有路由状态都通过 Task A 暴露的接口操作。

交付：网络层 module + 集成测试（用真实 UDP socket 和 WS 客户端模拟
收发，覆盖第9章里"包长度不足"、"version不识别"、"心跳超时踢出"三类场景）。
```

---

## 4. Task C：可观测性 / 运维 Agent 指令

```
你负责实现 PRD 第8章的可观测性与运维能力，独立于业务逻辑。

你要实现：
1. 用 tracing 初始化日志，分级：INFO/WARN/ERROR，具体埋点位置由
   Task A/B 预留的回调触发，你只需要定义回调签名和日志格式规范，
   写清楚文档告诉 A/B 在哪些事件上调用。
2. 用 metrics + metrics-exporter-prometheus 暴露 :9090/metrics，
   实现第8.2节列出的5个指标。
3. 实现优雅关闭：监听 SIGTERM/SIGINT，通知 Task B 停止 accept 新连接，
   等待现有队列排空或超时后强制退出。

交付：可观测性 module，包含一份"埋点接口规范"文档，供 Task A/B 对接。
```

---

## 5. Task D：客户端 SDK 示例 Agent 指令

```
你负责实现 PRD 第6章要求的客户端参考实现，用于验收标准第4条
（"提供最小可运行示例"）。

交付：
1. 一个 Python 脚本，模拟 AI 客户端：连接 WS、按第4章协议组装
   AI_EVENT 包发送、能接收 RAW_MOTION 广播并打印。
2. 一个最小 HTML+JS 页面，模拟前端：连接 WS、能接收广播、能发送
   UNICAST 包。
3. 两者都要展示 Sequence 号的正确递增，作为"如何处理丢包/乱序"
   的参考实现（PRD 第6章要求，但网关本身不保证）。

明确不要做的事：不要实现鉴权/校验逻辑，因为网关本身没有这两项。
```

---

## 6. Review/QA Agent 指令（闭环的关键）

```
你是最终验收人。你不写业务代码，只负责拿 PRD 第9章（边界情况清单）
和第11章（验收标准）逐条核对已集成的代码库。

流程：
1. 对第9章的每一条，写一个对应的测试用例（如果 Worker Agent 没写全），
   跑一遍，记录 Pass/Fail。
2. 对第11章的每一条验收标准，实际执行（压测、kill -9 模拟、跑
   Task D 的示例脚本），记录 Pass/Fail。
3. 输出一份结构化报告：
   | 条目 | 来源章节 | 状态 | 失败原因（如有）| 建议归属哪个 Task 修复 |
4. 只要有一条 Fail，就不允许判定"完成"，把报告原样返回给 Coordinator。
5. 全部 Pass 后，额外检查一次"非目标"（第10章）有没有被过度实现——
   比如是否有人偷偷加了鉴权或 CRC 校验，这也算不符合需求，一并打回。

不要自己修代码，你只出报告。
```

---

## 使用建议

- 如果你在 Claude Code 里操作：Coordinator 就是你的主对话，Task A~E 和 Review 可以用 subagent（Task tool）并行拉起，各自给独立的任务包 + 协议契约作为 prompt。
- 如果是手动多窗口协同：按顺序先跑 Task A/B（核心依赖），再跑 C/D/E（可并行），最后过 Review。
- 每轮迭代给 Review Agent 的报告要**结构化**（表格形式），这样 Coordinator 转发给 Worker Agent 时不会丢失上下文，也方便你自己人工抽查。
