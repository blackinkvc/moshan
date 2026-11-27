---
name: pocket-bridge
description: 本机「个人中台」的接入说明。用户提到中台 / 工作台 / 记账 / 设备 / 存图 / 阅读 / 工作日志，或要把这一轮干的活记进中台、查中台现状时使用。首次使用会先核对数据目录并把结论回报给中台。
---

# 中台桥（@@A@@ → 个人中台）

这台机器上有一个「个人中台」，是本机数据的总台：账号账单、设备台账、存图、阅读、
工作流水、各 AI 客户端干的活，都在它那儿。它现在想知道**你（@@A@@）的数据放在哪**，
以后好自己来取。

## 一、它是什么

| 项 | 值 |
|---|---|
| 服务地址 | `@@B@@`（本机回环免令牌） |
| 页面 | `@@B@@/dash` |
| 给 agent 的说明书 | `@@B@@/agent` |
| 能力清单 | `@@B@@/api/agent/schema` |
| 现状摘要 | `@@B@@/api/agent/brief` |

能做的三件事：**报这一轮干的活（带 token 用量）、报用量账、读中台现状**。
字段和口径全在 `/agent` 里，别自己发明新字段。

## 二、第一次：把你的数据目录告诉它

它先猜了一份，你核对：

```bash
curl -s "@@B@@/api/agent/link/scan"
```

看看 `agents[].evidence` 里那些路径，你自己机器上是不是这些地方。中台按已知的几家
预填了候选（Trae 的 `~/.trae-cn/memory/projects`、Codex 的 `~/.codex/sessions`、
Claude 的 `~/.claude/projects` 等），**你用的是别家、或者目录改过，就把你自己的目录报上来**：

- 有「一轮一轮的会话记录」的目录最好（jsonl / json 都行，能读就行）
- 整库加密的（比如 Trae 的 `database.db`）别报，读不到正文，只会白扫
- 实在没有可读的历史记录也不要紧 —— 第三节的自报就能把活记进中台

**核对完把结论回报给它**（路径要真存在；新增的你也可以补上）：

```bash
curl -s -X POST "@@B@@/api/agent/link/report" \
  -H 'content-type: application/json' \
  -d '{"agent":"@@A@@","paths":[{"key":"memory","path":"<你确认的路径>"}],"note":"<一句话说明>"}'
```

Windows PowerShell 版：

```powershell
Invoke-RestMethod -Uri "@@B@@/api/agent/link/report" -Method Post `
  -ContentType 'application/json' -Body '{"agent":"@@A@@","paths":[...]}'
```

回报完它就能自己导了（你不用管，它按你给的路径读）：

```bash
curl -s -X POST "@@B@@/api/agent/link/import" -H 'content-type: application/json' -d '{"agent":"@@A@@"}'
```

## 三、以后每一轮：报「干了什么 + 花了多少」

**干完活收尾时必须报一笔**（用户说「记一下」也算）。这条进中台「工作台 → AI 工作」
那张卡，**按 agent 分组显示**，`agent` 字段填你自己的名字（比如 `@@A@@`），
不填就会被记成「未知」。本机回环免令牌，不用带 token。

```bash
curl -s -X POST "@@B@@/api/agent/note" -H 'content-type: application/json' \
  -d '{"agent":"@@A@@","task":"<80 字以内，这轮干了什么>","summary":"<400 字以内，结论/改了什么>","project":"<项目名或目录>","model":"<模型名>","in":<输入 token>,"out":<输出 token>,"calls":<工具调用次数>}'
```

Windows PowerShell 版：

```powershell
$b = @{agent='@@A@@'; task='<这轮干了什么>'; summary='<结论>'; project='<项目>';
       model='<模型>'; in=1200; out=800; calls=6} | ConvertTo-Json
Invoke-RestMethod -Uri '@@B@@/api/agent/note' -Method Post `
  -ContentType 'application/json' -Body $b
```

老接口 `@@B@@/api/trae/note` 仍然可用（等价于 `agent` 填 `TRAE`），新接的别用它。

| 字段 | 必填 | 说明 |
|---|---|---|
| `agent` | 建议 | 你自己的名字，卡片按它分组。缺省记成「未知」 |
| `task` | 是 | 一句话：这轮干了什么。同一条任务 5 分钟内重复报会被服务端丢掉 |
| `summary` | 否 | 结论、改了哪些文件、留了什么坑。**写结论，别抄对话过程** |
| `project` | 否 | 项目名或目录，卡片上按它分堆 |
| `model` | 否 | 实际用的模型名 |
| `in` / `out` | 否 | 输入 / 输出 token。**估算值就够，估不出来写 0，不要编数字** |
| `cached` | 否 | 命中缓存的输入 token |
| `calls` | 否 | 这轮工具调用 / 步数。没有就省 |

还想让「用量账本」按客户端、模型汇总算成本，再补一条（可省）：

```bash
curl -s -X POST "@@B@@/api/agent/usage" -H 'content-type: application/json' \
  -d '{"client":"@@A@@","model":"<模型>","in":<估>,"out":<估>,"calls":<轮数>,"task":"<20 字>"}'
```

产出值得留档的（定了方案、改了脚本），再往总线加一条 —— **这条是给别的 agent 读的，
不能替代上面那条工作账**：

```bash
curl -s -X POST "@@B@@/api/agent/event" -H 'content-type: application/json' \
  -d '{"ns":"工作","type":"task","title":"<一句话>","text":"<两三句结论>"}'
```

两条铁律：

1. `task` 写具体动作（「把 dmg 的校验值并进 release.json」），不要写「完成了任务」。
2. in/out 估不出来就写 0、只填 `calls`。**宁可显示 0，也不要编一个像样的数字。**

## 四、边界

- 只走 `@@B@@`，别改 `dash.html`，别动中台其它端口的配置。
- 只追加不删改：要更正就再写一条带 `ref` 的。
- 不要把令牌写进代码或日志。
