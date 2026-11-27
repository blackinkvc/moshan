# 个人中台 · Agent 读写说明

这个中台是反重力猫头鹰的本机数据台。你是外部 agent（Trae / Cursor / 脚本），
下面是你**唯一需要知道**的一套接口。不要直接改 dash.html 或 pocket 的代码。

## 0. 怎么连

本机：`http://127.0.0.1:8323`（明文口，回环免令牌）
别处：同上地址换成主机 IP，并带 `-H "x-token: <令牌>"`（令牌在 pocket/config.json）

先自检：`curl -s http://127.0.0.1:8323/api/ping`

## 1. 写用量（trae 消耗的 token 就报这里）

```
POST /api/agent/usage
Content-Type: application/json

{"client":"trae","model":"claude-sonnet-4.5",
 "in":12000,"out":3400,"cached":0,"calls":3,
 "task":"改合表列序","ref":"会话id或链接"}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| client | 是 | 哪个客户端在报：trae / cursor / claude-code / 你的名字 |
| in / out | 至少一个 >0 | 输入 / 输出 token；不知道就填 0，但不能全空 |
| cached | 否 | 缓存命中的输入 token |
| calls | 否 | 请求次数，缺省 1 |
| model | 否 | 模型名，缺省「未知模型」 |
| ts | 否 | 时间，缺省现在。支持 epoch 秒/毫秒、`2026-11-21T21:50` |
| cost | 否 | 真金白银（元）。订阅制不知道就不填，别自己编 |
| id | 否 | 幂等键。不填按内容指纹去重，同一任务一分钟内重复 POST 只算一次 |
| ref | 否 | 会话 id / 链接，方便回查 |

批量：`body` 直接给数组（最多 200 条）。

## 2. 写事件（干完活留一条）

```
POST /api/agent/event
{"ns":"设备","type":"note","title":"给空调加了定时",
 "text":"……","data":{"k":"v"},"tags":["自动化"]}
```

`ns` 是归档槽位，建议用现有这些，别新造同义词：
`设备` `伙食` `账目` `工作` `阅读` `系统` `杂项`

`type`: note / metric / task / link / file / span。

## 3. 读

```
GET /api/agent/read?ns=all&since=7d&limit=100&client=trae&q=关键词
```

`ns`: all / usage / events。`since`: `24h` `7d` `today` 或具体日期。

## 4. 现状摘要（别一个个接口翻）

```
GET /api/agent/brief
```

返回今天的 token、花费、最近工作、设备数、自报账最近几条。

## 5. 数据在哪

@@ROOT@@

  @@ROOT@@/_工作台/agents/usage.jsonl
  @@ROOT@@/_工作台/agents/events.jsonl

两份都是 append-only 的 JSONL，一行一条。你可以直接 tail / grep ——
写接口和磁盘是同一份数据，不存在同步问题。

## 6. 中台里还有什么

先读 `@@ROOT@@/README.md`（完整地图）。常用的几处：

    13-设备/档案/       设备台账，一台一个 json
    09-个人工作台/      待办 / 行程 / 备忘 / 日记
    12-记账/            账目
    04-存图/ 08-阅读/   存下来的图文
    _工作台/dash.html   工作台页面本体（别手改；改前先 cp 一份 .bak）

要全量状态而不只是摘要：`GET /api/dash?k=<令牌>` —— 几百 KB，别每轮都拉。

## 7. 不想拼 curl？用命令行

```
@@ROOT@@/_工作台/agents/zt brief
@@ROOT@@/_工作台/agents/zt usage --client trae --model claude-sonnet-4.5 --in 12000 --out 3400 --task "改列序"
@@ROOT@@/_工作台/agents/zt log --ns 工作 --title "给合表加了列拖拽"
@@ROOT@@/_工作台/agents/zt read --ns usage --since 7d --limit 5
```

（`zt` 就是个包装层，底层还是上面那几个接口；从别的机器调时加
`--base https://<主机>:8321` 并设 `ZT_TOKEN=<令牌>`。）

## 8. 给中台加一个第三方程序

中台的插件分两种：自己的服务（有端口、有 .command）走 `plugins.json`；别人写的
桌面程序走「配方」—— 一条 JSON 说清怎么认装没装、怎么装、怎么开退。

    GET  /api/ext/spec            # 字段说明 + 示例 + 提示词（JSON）
    GET  /api/ext/prompt?goal=    # 纯文本提示词，可直接喂给模型
    POST /api/ext/parse           # {"text":"<模型回答原文>"} → 配方草稿（不落盘）
    POST /api/ext/add             # {"recipe":{...}} → 落盘并加进顶栏

提示词模板在 `@@ROOT@@/_工作台/第三方插件-Prompt.md`（可改）。最容易出错的
是 `brew` 包名：桌面程序是 `brew install --cask <名>`，命令行是 `brew install <名>`；
不确定就别写 brew（用户装完照样能用），**不要编一个包名**。

## 9. 不要做的

- 不要写超过 4000 字的正文；大段材料放文件，事件里只放路径。
- 不要 `PATCH` / 删除：只追加。要改就再写一条新的（带 `ref` 指回旧的）。
- 不要把令牌写进日志或提交到仓库。
