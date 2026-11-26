# 更新日志

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [0.3.12] - 2026-11-26

**Agent 监控这一页被拆开重排了：一个指标一张卡，不再挤成一张大表；新加的「母任务」
把几十条会话按侧栏分组归拢，能一路点到某天说过的那句话。折叠栏终于不会被裁掉半截，
换新版本时也不会再把旧页面当成新页面发出去。**

### 新增
- **新卡「母任务」**（Agent 监控页，紧挨着那张卡）：一行一个母任务 —— 母任务就是
  WorkBuddy 侧栏那个分组（空间：个人中台 / 墨山插件 / 新闻摘录 …），点开是名下那
  十几条会话（子任务），再点开是那条会话里说过的话。六个视角都是同一份
  `/api/tasks`：**列表**（母 → 子 → 说过的话三级）、**甘特**（母任务一根汇总条，
  展开后子任务各一根，色深 = 当天烧的 token，斜纹 = 动过但没 token 账）、
  **消耗**（母 / 子两档排行）、**热图**（母任务 × 天）、**成本**（按计费卡的混合
  单价折 ¥，另列「在办但 7 天没动」的僵尸任务）、**效率**（每条 / 每天 token）。
- **窗口口径一律跟着范围走**：7 天 / 30 天 / 全部切过去，token、轮数、天数全都
  只算这个范围。任务对象上原来的 `tok` 是这条会话**一辈子**的累计，直接拿来用的话
  切档数字纹丝不动（实测 7 天和 30 天都是 8.37B，看着像没生效）。
- **「提示词」卡能按字、也能按 token 看**（卡头两颗小芯片切换，记住了下次还是它）。
  「字」是原文的**字符数**：一个汉字、一个字母、一个标点各算 1，不是字节也不是
  token —— 以前只写「字/条」，看不出数的是哪种。「token」是同一段原文按 DeepSeek
  官方换算率折的估数（中文 0.6 / 其他 0.3），中文占比是扫描时逐字数出来的，中英
  混排也准。**它不是**「每条 token」那张卡的实测 usage：usage 里的 inputTokens 是
  整个上下文（一条几百万，含缓存重发和几十轮工具调用），拿它当「这句话多长」会差
  三个数量级，所以卡上写明「折算值 · 非实测」。
- **Agent 监控页补齐八张逐日 / 逐时卡**：活跃度格子（四种口径）、分小时、屏幕时间、
  提示词分时、链接与图片、每条 token、每条耗时、分 agent —— 一个指标一张，不并 tab。
- **这台机器卡的两行小指标能收能展**：收起时只留标题和一个数字，展开后按实际行数
  把卡片撑开 —— 展开期间自己接管 `grid-row`，不再被位置存档写下的内联值压住。

### 修复
- **折叠栏展开被裁掉一半**（用户报「卷轴展开的时候被挡住」）。三个漏一起补：
  服务端按页内 `moshan-page` 戳判断要不要换页，戳没变就一个字节都不动 —— 数据根
  那份比包里落后 620 行照样发出去，现在戳相同也比内容；位置存档会在卡上留内联
  `grid-row`，优先级高于 `--hcell`，展开多高都不生效；行高按常量 72 估，N 行实际是
  `72N-16`，恒定少算十几像素 —— 改为实测 `--row` + `row-gap`，落一次再按实际差补行。
- **进程日志不再每 5 分钟无条件写一行**：一天 288 行里绝大多数是同一批进程的重复。
  改成内容有变化才写，没变化每小时保底留一条 —— 否则「机器闲了一整天」和「采集挂了」
  在日志上分不出来。
- **页面自动升页的备份从明文换成 gzip 归档**（`_bak/YYYY-MM/页面名.时间.gz`，约 3 倍
  压缩）：历史一份不删，按年月分目录，免得根目录堆几千个文件。首次落地把已有的 75 份
  一次性转档，46M → 15M。

### 改动
- **界面上的「本机工作」全部换成「Agent 监控」**：系统地图节点、编辑态的加卡按钮与
  隐藏池来源标签、计费报错、进程榜链接、功能文档、`/hostwork` 说明，共八处。数据目录
  `_本机工作` 和 AI 工作计划里的项目名没动 —— 那是账，不是标签。
- **数据根目录 `16-多机` → `16-虫巢思维`**：仓库里的路径常量还是旧名，只改了运行副本，
  下次回灌就会把旧名覆盖回去、中台读不到 `fleet.db`。`fleet.py` / `dash.py` /
  `server.py` / `host_push_update.py` 的默认值一并纠正。
- 撤掉没走通的 WALLGRID 统一排版实验（565 行脚本整段移除）。

## [0.3.11] - 2026-11-25

**先把「这台机器到底装了什么」说清楚：没装的不再占一格红点，装完没弹出的
安装盘一眼认出来，另外开始记「我从哪天开始用、之后跑过哪些版本」；桌面冒出一串
黑框的时候，也终于有人替你留下证据。**

### 新增
- **版本足迹**（设置 → 关于 → 版本足迹）：首次启用时间 + 之后跑过哪些版本，
  每个版本的首末时间和启动次数，点开看明细。数据落在状态目录的 `versions.json`，
  启动时记一笔，**不联网**，断网也能看。首次启用时间是按配置文件 / 应用包的
  创建时间推的，页面上标「不晚于」，不冒充精确值。
- **终端风暴自动留证**：桌面上一串黑框，过去只能靠人回忆。2026-11-25 01:13
  EVO 上终端进程内存从平时的 103MB 涨到 6673MB、孤儿 conhost 六百个，
  事后翻遍记录只查到一句「好像弹了很多」。现在进程采样每 5 分钟被看一遍，
  终端类进程（WindowsTerminal / conhost / OpenConsole / Terminal / iTerm）
  合计内存过了 1200MB（平时十倍）就落一条：起止时间、峰值、那一刻最占内存的
  几个进程，写进「本机工作」的异常账，工作台卡片也读得到。同一轮风暴合并成
  一条，回落之后再次爆发算两条 —— **只记事实，不猜是谁干的**。
- **安装盘（.dmg）识别 + 一键推出**：磁盘卡里挂着的磁盘映像标「安装盘」，
  选中它就出现推出键。一排 `Moshan 0.3.4 / 0.3.5 / 0.3.8` 不是版本历史，
  是三个装完没弹出的安装包 —— 现在能直接推掉，不用回访达一个个找。
  （只推磁盘映像，外置盘会拒绝。）

### 修复
- **本机没装的服务不再显示成「掉线」**：巡检那排胶囊和「服务方阵」按
  「装没装」过滤 —— 判据是启动脚本在不在、launchd 有没有挂号、基础服务
  （gateway / ollama / qdrant）有没有可执行文件。没装的收进
  「还有 N 项本机没装」，点开才摊出来（灰点 + 点线边，鼠标停上去说清原因）。
  顶栏「N 个服务没在线」、全链路图的「探活 N 个」也改成同一口径，
  不再拿没装的东西当故障报。
- **巡检服务自己没起来时给一句解释**：卡片上的心跳/磁盘/内存是口袋自己采的，
  所以 8774 挂着红灯时页面照常有数 —— 这句说明直接写在卡里，
  并指出「跑一轮 / 自愈」为什么点了没反应、该去哪个目录看原因。

## [0.3.10] - 2026-11-25

**对话用哪个模型，现在自己选：本机现成的、现场新拉的、或者接一个外面的 API。**

### 新增
- **Agent 监控多一面「配置」**（分组 / 任务 / 插件 / Skill / MCP，一家一份）。
  和「数据面」分工：那边数几万个会话文件，几百毫秒到几秒；这边只读几个清单和小库，
  通常 1 秒内，带 180 秒缓存。全部只读（sqlite 走 `mode=ro`），单面失败不牵连其余
  四面；读不出来的字段留空并写进 `notes` —— TRAE 的分组目录名里中文被替换成了 `-`，
  还原不回来就照实说，不猜。
- **机器规格能按子项开关**，另加一张磁盘浏览卡。
- **引导页「本机大模型」拆成三条路**（勾上那个开关就展开）：
  - **本机已有** —— 把 Ollama / LM Studio / 本地网关探到的模型摊平成一个下拉，
    选中点「用这个」立刻生效。以前只有一个写死的 `qwen3.5-4b-gguf:q4km` 输入框，
    这台机上明明有 35B 的专家模型也看不见、用不上。
  - **下载新的** —— 原来的 `ollama pull`，拉完自动切成「用这个」。
  - **接 API / 网关** —— 任何 OpenAI 兼容端点（LM Studio、vLLM、one-api、
    云厂商），地址带不带 `/v1` 都行，可先「测试连接」看它能列出哪些模型。
  三条路都走 `POST /api/model/save`，选完当场生效 —— 只写 config.json 是不够的，
  进中台后客户端还拿着旧模型名，得等重启，用户会以为已经选好了。
- **中台「设置 → 对话模型」是同一套入口**：本机已有的下拉、地址 / 密钥 / 模型名
  三个框、测试连接、保存并使用。保存后直接贴到正在跑的客户端上，**不重启进程**。
- 新模块 `moshan/modelcfg.py`（来源清单、地址归一、探测、密钥打码、原子落盘）
  与三个接口：`GET /api/model`、`POST /api/model/test`、`POST /api/model/save`。
  密钥只回打码串；页面留空保存表示沿用原值，不会把真 key 冲掉。

### 修复
- **端点没有 `/mcp` 时不再整轮失败**。以前换到纯 OpenAI 端点，工具目录一拉就抛
  `GatewayError`，对话直接报错。现在探不到工具就降级成纯对话，并明确告诉模型
  「你没法读写本机文件，别编」；TTL 内不再重探，不会每轮白等一个超时。
- **思考型模型不再交白卷**。Qwen3 这类会先把 token 花在 `reasoning` 上，
  `content` 是空的 —— 实测 `max_tokens=800` 时回复是「（模型没给出文本）」，
  看着像端点坏了。现在取思考结尾兜底，`max_tokens` 上限提到 2048。
  mac 直连 Ollama 实测：`1+1 等于几` → 「1+1 等于 2。」，`finish_reason=stop`。
- 系统提示不再把机器说成「这台 Mac」，Windows 上按平台说。

## [0.3.9] - 2026-11-25

### 新增
- **设置里能导入 AI 历史账了**（`设置 → AI 历史账 → 扫描 / 导入`）。把 WorkBuddy
  和 Cherry Studio 已有的历史会话折成基线账（会话数 / 调用次数 / token / 常用模型 /
  实付金额）写进数据根的 `_本机工作/Agent自述.jsonl`，卡片立刻能看到。以前这些账
  是每次刷新现算的 —— 换台电脑只拷了数据根、或者哪天清了 `~/.workbuddy`，历史就
  凭空消失了。导入只读外部目录、不搬原始会话，重复导入不叠加。
- **数据根可以一键在访达里打开**（设置 → 关于 → 数据根 →「在访达中打开」）。
  以前只能照着路径自己去 Finder 里一层层点。

### 修复
- **「谁在干活」整块空白**：采集层只要有一处抛异常，整份 `/api/overview` 就降级，
  客户端列表、token、机器指标全变成空表。现在降级时把完整堆栈写进日志（只回一句话
  给前端，事后根本查不到哪一行），并把 `os.statvfs` 的 `AttributeError` 兜住 ——
  插入一个可弹出盘就让整页空白，这条以前在 Windows 上真发生过。

## [0.3.8] - 2026-11-25

**从这一版起，macOS 上更新不用再下载安装包：点一下「立即更新并重启」，它自己换掉自己。**

### 新增
- **应用内原地更新**（macOS）。以前点「安装」是打开 dmg 再让人把图标拖进
  「应用程序」—— 每更新一次都要重做一遍，这不叫自动更新。现在：
  1. Release 除了 dmg 还发一份 `Moshan-*-macos-*.zip`（`ditto -c -k` 打的，
     签名、符号链接、扩展属性都在；用 `zip(1)` 会把符号链接存成普通文件，
     解出来签名当场坏）；
  2. 更新器在 macOS 上优先取 zip，解到状态目录，先 `codesign --verify --deep
     --strict`，验不过直接放弃；
  3. 起一个脱离会话的 helper 脚本：等本进程**真的退出**之后，把 `.app` 挪成
     `.moshan-old`、`ditto` 新的过去、清掉 quarantine、重新打开；复制失败就
     把旧的挪回来。任一段失败都不会留下半个坏软件，日志在
     `状态目录/updates/apply-<时间戳>/apply.log`。
- 关于页的按钮按能力说话：能原地更新时是「立即更新并重启」，不能时仍是
  「打开安装包」，并在提示里说明原因。

### 注意
- **0.3.7 及更早的版本里没有这套逻辑，所以 0.3.7 → 0.3.8 仍要手动装一次。**
  从 0.3.8 开始，之后的版本都能在软件里点更新。
- 不能原地更新的几种情况会如实说明，而不是假装成功：app 还在 dmg 里运行、
  被 Gatekeeper 挂到 `/private/var/folders/.../AppTranslocation/`（从下载目录
  直接双击运行时会发生）、应用目录不可写、包没有校验值。
- Windows 仍然走 exe 安装器（运行中的 exe 被系统锁着，换不掉自己），但那是
  覆盖安装，不需要重新下载。

### 顺带
- `updater.cleanup()` 连原地更新留下的解压目录一起清，只留最近两份（日志能查）。

## [0.3.7] - 2026-11-25

**Windows 上「一晚上弹了上千个终端窗口」，根因是子进程没关窗口 + 默认终端是
Windows Terminal。这一版把这条路堵死，并补齐插件表在 Windows 上的识别。**

### 修复
- **从 GUI 进程起控制台程序会在桌面上弹黑框**。Moshan.exe 本身没有控制台，
  它去起 `powershell.exe`、`python.exe`、带 `.cmd` 的脚本时，系统会给子进程
  新建一个控制台窗口；本机「默认终端」若是 Windows Terminal，还会经 COM 交接
  再拉起一个 `OpenConsole.exe`。轮询/刷新一次闪一个，看上去就是满屏弹窗。

  2026-11-25 在 EVO 上实测到现场：`conhost.exe` 544 个、`OpenConsole.exe` 397 个，
  且 6 秒内还在新增 21 个，其中 392 个 conhost 的父进程早就退出了 —— 全是孤儿。
  真正的发起者不是中台（它那 5 秒里 CPU 只涨 0.1 秒、没拉子进程），是另一个
  程序在每秒跑一次 `git log`；但**中台自己起 powershell 探测时同样会弹**，
  这条隐患必须堵掉。

  现在统一走 `paths.hide_console()`（POSIX 下返回空 dict，等于不带任何开关）：
  `hostwork`、`sysinfo`、`pyexe`、`bootstrap`、`heartbeat`、`dedupe_runner`、
  `ai_trends_runner`、`articlebox`、`server` 的 snapshot 调用全部带上
  `CREATE_NO_WINDOW`。控制台照旧存在、stdout 照旧能取，只是不显示窗口。
- **「第三方插件」表在 Windows 上一律显示「这台没装」**。原来只认
  `/Applications` 下的 `.app`、brew 目录和 PATH 里的命令，这三样 Windows 上都没有。
  现在补了三条 Windows 判据：注册表「应用和功能」清单（十分钟缓存，覆盖
  scoop / winget / 用户目录装的）、进程名精确匹配（走 psutil，不用命令行匹配）、
  本地端口应答。版本号也直接从注册表的 `DisplayVersion` 取。
- **首次全量扫描把接口堵死**（0.3.5 的 `c9e872d` 已修，这里只是记一笔）：新数据根
  没有会话缓存时要扫 40~90 秒，而页面 20 秒就 abort —— 表现是「进程消耗」「本机
  AI 的工作」两张卡一起空。改成后台补扫、本次请求不等待。

### 说明
- 四档端（橙主端 / 紫自管 / 蓝受限 / 绿只读）**共用这一个版本号**，功能与权限
  按档位在运行时区分，不按档位分叉版本号。详见 `docs/ARCHITECTURE.md` 第八节。
- 「本机大模型」卡空白不一定是坏了：LM Studio / Ollama 停掉后端口没人应答，
  卡片就会显示「没在跑」。服务在、只是没加载模型时，卡片会单独说明。

## [0.3.6] - 2026-11-25

**双击打开应该是中台，不是口袋的文件列表。口袋只是中台里的一项能力。**

### 修复
- **装完打开落在口袋首页**（`/`，一个文件列表 + 抓取），中台要自己找路过去
  —— 而新装的机器上 `/dash` 还是 404，路根本不通。两处都在：
  1. `launcher.py` 启动后打开的一直是 `/?k=…`；`welcome.py` 配置完成后也跳回 `/?k=…`。
     两者都改成 `/dash?k=…`（令牌仍走 `?k=` → `pocket_token`，两页共用一份）。
  2. **中台页面从来没有随包分发**。`dash.html` 只存在于作者本机的数据根
     `_工作台/`，别人装上后 `/dash` 返回「工作台页面还没生成」—— 于是一个中台
     卡片都看不到，只剩口袋界面，看上去这软件就是个网盘。
     现在 `moshan/dash.html` 进仓库、进安装包；`/dash` 发现数据根没有就从包里铺一份。
     **只「缺了才铺」**，用户改过的页面一个字节不动。

### 注意
- 默认落点变了：双击是 `/dash`（工作台）。口袋仍在 `/`，也从工作台顶栏的插件条进得去。
- 首次安装仍先进 `/welcome`（配模型），配完直接落到中台。

## [0.3.5] - 2026-11-25

**0.3.4 的 dmg 双击报「已损坏，移到废纸篓」，装不上；图标也不见了。两个都是打包链路的病。**

### 修复
- **安装包双击「Moshan.app 已损坏，你应该将它移到废纸篓」**（0.3.4 所有 macOS 包）。
  三个原因叠在一起：
  1. **运行期把数据写进了 .app 内部**。`transfer.py` 的账本路径写成
     `Path(__file__).parent / "pocket.jsonl"`，打包后 `__file__` 指向
     `Contents/Frameworks`，app 一跑就往自己肚子里写文件 —— 而那是代码签名覆盖的范围，
     写一个字节 seal 就失效。CI 又把「冒烟测试」放在签名之后，于是发出去的每个 dmg
     都带着坏签名。现在可写路径统一走 `paths.base_dir()`（源码运行 = `moshan/`，
     打包后 = 状态目录），只读资源走 `paths.resource()`。
  2. **`codesign … || true`**：签名失败也照发。现在失败即红。
  3. **dmg 用 `cp -R` 复制 app**：macOS 的 `cp -R` 会跟随符号链接（bundle 里有十几个）、
     丢扩展属性，复制出来的 app 签名当场失效。改用 `ditto`。
- **图标不见了**（Dock / 访达 / 安装器都显示通用 Mac 图标）：`spec` 里两处 `icon=None`，
  PyInstaller 就退回自己的默认图标。现在仓库里带 `packages/moshan.icns`（macOS）与
  `packages/moshan.ico`（Windows），由 `docs/images/logo.svg` 生成。

### 新增（CI 把关，防止同类问题再发出去）
- 打包后记录 bundle 文件清单，冒烟跑完 diff：**有新增文件就失败**，并直接提示
  「写数据要走 `paths.base_dir()`」。
- 签名后 `codesign --verify --deep --strict` 必须过；做完 dmg 再挂载回来验一次
  —— 验的就是用户拿到手的那份文件。
- 签名前清掉 `.app` 里的 `._*`（exFAT/网络卷上 macOS 会撒这些，codesign 会把它们
  当子组件去签，然后报 `Operation not permitted` 整包失败）。

### 注意
- 安装版的数据位置变了：`pocket.jsonl`、`config.json`、`certs/` 从「app 内部」挪到
  状态目录（macOS 是 `~/Library/Application Support/Moshan/`）。旧版写在 app 里的那份
  账本随旧 app 一起删掉即可 —— 它是两分钟一条的心跳流水，不是业务数据。

## [0.3.4] - 2026-11-25

**Windows 上「这台是什么机器」终于答得出来了：机型、芯片、应用清单、常驻服务不再是空的。**

### 修复
- **「机器规格」在 Windows 上整张卡是空的**（型号「未知机型」、芯片「—」、0 个应用、
  0 个常驻服务）。`specs()` / `software()` / `units()` 三个函数只写了 macOS 那条路
  （`sysctl` / `system_profiler` / `launchd`），Windows 上一条都取不到值。现在：
  `specs()` 走一次 `Get-CimInstance`（机型/芯片/显卡/系统/BIOS）翻成同一份形状；
  `software()` 读注册表三处 Uninstall 键（64 位 / WOW6432Node / 当前用户，毫秒级，
  口径与「设置→应用」一致），命令栏取 scoop / choco 的包目录；
  `units()` 用 `psutil.win_service_iter()` 列服务，只留「在跑」或「开机自启」的
  —— Windows 光 disabled 驱动就两三百条，全摊上去这张卡没法看。
  内存优先 psutil，没有则退 `sysinfo`（`GlobalMemoryStatusEx`，系统自带），
  不再因为缺一个包就把 16 GB 报成 0。
- **数据根加了「出生证」**：`_工作台/machine.json` 记下这份资料库是哪台机器产生的
  （有章不覆盖），`/api/dash` 多一个 `machine.foreign` 字段。起因是 EVO 的 `/dash`
  里显示着另一台机器 629 条工作流水 —— 部署时把 `09-个人工作台/流水` 整目录拷了过去，
  而程序本身分不出「这份数据是本机攒的还是别人搬来的」。现在拷贝会连章一起带走，
  换机器一读就知道来源不对。只报不拦，清不清由人决定。

### 新增
- **自述通道从「只认 TRAE」改成「认任何 agent」**：接口 `/api/agent/note`（`agent` 字段决定
  记到谁名下，缺省「未知」），老的 `/api/trae/note` 保留等价于 `agent=TRAE` —— 已经装出去的
  说明书不会失效。落盘从 `TRAE自述.jsonl` 换成 `Agent自述.jsonl`，老文件继续读，卡片按 agent
  分组显示（Codex、Claude Code、自建 harness 各占一行，不再挤在 TRAE 名下）。
- **认人表扩到 8 家**：Trae / Cursor / Claude Code / WorkBuddy / Codex / Gemini CLI /
  OpenCode / DeepSeek Harness。升级后说明书**投给每一家已装的**（没装的不造目录），
  不再是只塞 Trae 一个目录。
- 非 Trae 的几家不做日志硬解：只如实列出根目录下的 `sessions` / `projects` / `history`
  里有什么（几条、最后改动时间），导入走它自己按说明书自报 —— 各家会话格式一直在变，
  猜格式比不读更糟。
- **「每日工作总结」卡**（本机工作页，`workstat.py` + `GET /api/workstat`）：一张卡里四个分页
  —— 时长（AI 与人分小时、GitHub 那种活跃度格子、分软件屏幕使用时间）、Token（分小时 /
  分 agent / 分基础模型）、提示词（每天条数、分小时条数、字数分布、链接与图片数）、
  效率（平均每条提示词多少 token / 多少分钟 / 多少字，按 agent、专家、插件分开）。
  口径写死在模块顶部：AI 时长按「发请求那一分钟 + 后 2 分钟」的分钟格算，不是一轮首末跨度
  （后者会把挂一夜的窗口算成干活）；每条耗时是跨度但封顶 4 小时；平均 token 是窗口总量
  除以提示词条数。656 MB / 17 万行冷启动 2.5 s，之后按 offset 增量 + 60 s 缓存，命中约 70 ms。
  屏幕使用时间读系统的 `knowledgeC.db`；读不到（没给「完全磁盘访问」）就自己采前台样本兜底，
  并在卡上说明缺哪些天。

### 修复
- `trae_work.items()` 里有一段重复的 `def items(...)`（历史上误粘贴），后一个定义盖掉前一个，
  行为没变但读代码会被误导；删掉重复的那段。

### 计划
- Windows 上补缩略图：优先 Pillow（pip 装、进程内、Windows 轮子 7 MB），而不是 PowerShell + System.Drawing —— 后者每张图都要起一次 PowerShell，约 200 ms，列表页会明显卡（详见 docs/WINDOWS-PARITY.md）
- 应用内托盘图标与「退出」菜单
- 真正的原子更新（versions/ + current 符号链接，替换正在运行的程序）

## [0.3.3] - 2026-11-24

**一次安全与健壮性排查的结果：11 条，其中 4 条能把「装上它」变成「在机器上执行别人的代码」。**

### 安全
- **没校验值的安装包不再能一键执行**：GitHub 不回 `digest` 时下载照样成功，但 sha256
  根本没比过 —— 以前界面只写一行小字，`/api/update/apply` 照样打开它。现在 `verified`
  为假直接 409，要人在对话框里二次确认才放行。新增 `updater.is_verified()`。
- **装包路径按路径段判断**：`/api/pack/install-path` 原来用 `startswith` 比字符串，
  `/data/AI项目-evil` 能冒充 `/data/AI项目` 混过去，而这里装进去的是会被执行的插件代码。
  改成 `Path.relative_to()`。
- **上传不再整读进内存**：账本导入、产品图、文件传输、插件包预览/安装/装目录共 6 处
  都是 `file.file.read()`，一个请求就能把进程读爆。统一走新的 `_read_upload(limit)`
  （64MB / 256MB 两档），超限 413。
- **批量上传有了总量上限**：`/api/upload` 原来只限单文件 500MB、文件个数不限，等于没限。
  新增 `BATCH_UPLOAD_LIMIT`（2× 单文件上限）。
- **推送更新防压缩炸弹**：`/api/update/push` 先看 `Content-Length`，包体上限 64MB、
  解压落盘上限 512MB；超限时安装版清空壳、源码部署从备份回滚，再报 413。
- **插件退出不再拼 AppleScript**：bundle id 改为严格白名单（字母数字点横线），
  引号进不去就没法接 `do shell script`；`pkill -f` 的进程名同样加白名单并禁止 `-` 开头。
- **令牌可以不再出现在地址栏**：用 `?k=` 进来一次后发 `HttpOnly; SameSite=strict`
  的 cookie，之后浏览器请求靠它即可；`X-Token` 头与旧链接继续有效。
- **页面统一带 CSP**：`default-src 'self'`，只放行本机端口的 iframe（工作台卡片要用）。
  XSS 想往外传数据会被 `connect-src`/`img-src` 挡住。
- **证书建不起来时不再静默裸奔**：明文退化时页面顶部挂红色横幅，且主端口只监听
  `127.0.0.1`（想继续在局域网用就必须显式写 `host`）。

### 性能
- **服务探活并发 + 5 秒缓存**：原来一条 for 循环串着探，远程被控机（portproxy）
  不通时每条都要等满 0.35 秒超时。多机离线时 `/api/dash` 会明显变慢。
- **上传改同步接口**：500MB 写在事件循环里会卡住所有其它请求，改回 `def` 走线程池。
- **首屏内联缩略图 40 → 12**：40 张要串起 40 次 `sips` 子进程，响应体还要 base64 再涨
  33%。12 张够铺满首屏，其余前端并发拉（有磁盘缓存）。
- **「本机工作」和「巡检」点开不再等两三秒**：逐轮记录的缓存只有 8 秒，而重算一锅
  （WorkBuddy 那近 600 条要重新组装）要 1.5~2.5 秒，前端 15 秒轮询一次 —— 等于
  **每次打开都撞在重算上**，连带把首屏后面 7 个请求全堵住（实测内容出现要 2.7 秒）。
  改成**先用旧账、后台刷新**（`_rounds_revalidate`）：过期了也立刻回上一锅，
  另起线程补算，页面只等 0.5 秒。同时缓存按档位分桶，「今天」直接切「7 天」那锅，
  不再为同一批数据解析两遍。

### 新增
- **巡检页「磁盘文件」卡**（`diskscan.py` + `/api/disk/*`）：三个模式共用一份卷列表。
  **结构**一层层点进去，目录标「N 项」，点「算」递归要真实体积、点「全部算」把这一层
  一次算完；**应用**按 `.app` 各自的体积排序（`.app` 是目录，不单独算就显示 0）；
  **类型**按图片/视频/文档/压缩包归类，回答「这块盘里都是些什么」。
  三个刻意的取舍：只算一层（全盘递归要几分钟）、单次限时 2.5 秒（外置盘 stat 很慢）、
  **跳过挂载点**（macOS 的 `/System/Volumes/Data` 不算的话 `/System` 会显示 784GB）。
  结果缓存 90 秒，来回切 tab 不重复扫盘。

### 测试
- `scripts/check.sh` 的路由枚举改成递归取，兼容新版 FastAPI 的 `_IncludedRouter`
  （否则依赖升级后第 2 节会假红）。

## [0.3.2] - 2026-11-24

**「每天 8 点没收到简报」的真正原因找到了：一次失败就把当天作废。**

简报的定时推送是「8–11 点窗口 + 60 秒一跳」，设计上晚开机也能补。但抢牌文件
（`<日期>.push`）建出来之后**永远不删**，而下一跳的判定是「牌子不在才推」——
于是第一跳只要发送失败一次，后面每一跳都撞在 `FileExistsError` 上直接退出，
窗口补推形同虚设，只能等第二天。这个洞在 8 点那一刻网关抖动、网络刚唤醒、
或某一段文本被渠道拒收时都会踩到。

### 修复
- **失败后能自己接着推**：新增 `<日期>.sent.json`，逐段记录哪些已经发成功。
  下一跳只补**没发出去的那几段**，不发从头一遍 —— 手机上不会收到两份，也不
  会因为第 3 段失败把前 2 段白等一天。
- **给重试封顶**：一天最多打 5 跳（约 5 分钟）。网关整段窗口都连不上时记一条
  `brief_push_giveup` 就收手，不再每分钟上去碰一次。
- **补齐两个边角**：`build()` 算不出来也计入尝试次数（原来会每分钟重算并刷同
  一条错误）；全部段落其实已发完、只是进程在落账前被杀时，下一跳直接补账，
  不再重发。

### 同步
- **任务清单的缓存按档位隔离**（`hostwork.tasks_view`）：`_TASKS_CACHE` 以前不
  分 range，切到 7d/30d 可能拿到上一档的内容。缓存同时从 20 秒压到 6 秒、补扫
  阈值同步改短 —— 增量扫描只要十几毫秒，没必要让这张卡比首页慢一拍。
- **插件清单第四档「其他插件」**（`dash.BUILTIN_APPS` / `plugins.json`）：以前只有
  「小工具 / 内置插件」两档，写了自己有数据、自己一套页面的东西（Token 价格追踪、
  智能设备、界面评审、月相、玄枢号看板…）只能硬塞进「内置」，把运维那一档撑得看
  不出重点。新档 `group: "other"` 专放这类；卡片型工具也可以在卡片上写
  `data-tool-group="other"` 自己声明，不用往后端写死 id 名单。
- **`POST /api/notes/add`**（`dash.add_note`）：给页面之外的调用方追加一条待办 /
  备忘。整份覆盖写的 `/api/notes` 不适合脚本和设备事件 —— 读完到写回之间页面自动
  保存一次，这条就没了。新接口在同一把锁里读-改-写并原子落盘；带 `key` 的调用幂等
  （同 key 已存在直接回 `dup`，轮询类调用一天问几百次也只加一条）。

### 说明
- 牌子（`<日期>.push`）**成功失败都不删**。删掉的话，另一个实例会在同一分钟
  接着抢、接着发，手机上就多一份。「失败要能重试」靠的是 sent 账，不是放牌。
- 判定逻辑从 `scheduler_loop` 里拆成 `_tick()`，返回值（`done/wait/busy/retry/
  giveup`）只为让这条链路能被离线测：五个场景（部分失败续推、连败封顶、已发
  未落账、当天已推、窗口外）在临时目录里跑，不发一条真实消息。

## [0.3.1] - 2026-11-24

**Windows 版从「明文 HTTP」变回 HTTPS。** 这条是安全问题，不是功能：没有证书
就生成不了证书，`server.py` 把 scheme 定成 http，令牌以 `?k=` 明文走局域网 ——
同网段谁抓一次包，就拿到整台中台的读写权。

### 新增
- `tls.py` 增加 **cryptography 通道**：证书的生成、解析、校验全部在进程内完成，
  不依赖任何外部可执行文件。Windows 上没有 `openssl` 命令，原来 `_generate()`
  直接返回 None，于是必然降级；现在这条路补上了。（mac 上原有的 openssl 路径
  保留当首选，它跑得久、没出过事。）
- `_valid()` / `_cert_covers()` 也走 cryptography：以前没有 openssl 时这两个函数
  一律返回 True，意味着「证书剩 0 天也不重签」「IP 变了也不重签」—— 浏览器随即
  拒绝连接，而程序自己不知道。
- **源码部署可以从 GitHub 更新**（`/api/update/apply`）：取 codeload 的源码包、
  削掉顶层 `moshan-<ver>/` 壳，再交给 `pushupd` 原地替换。EVO 那种「run-evo.cmd →
  源码目录」的部署，装个 exe 只会变成两个服务或自启指着旧目录，所以它需要的是
  这条路而不是安装包。
- `packages/moshan.spec` 收 `vendor/openssl/`（有就打包，没有就跳过）。

### 修正
- `_openssl()` 先看**程序目录**再看 PATH。打包版把 openssl.exe 放身边，PATH 里
  没有它 —— 以前「打进包」等于白打，打进去了也找不到。
- `lan_ip()` 在 Windows 上不再跑 `ipconfig getifaddr`（那是 macOS 的命令，
  Windows 的 `ipconfig` 没有这个子命令），直接走「连外网看自己用了哪张网卡」。
- `_iface_ip_by_prefix()` 优先用 psutil 取网卡地址（Windows 上没有 `ifconfig`）。
- `local_hostname()` 在 Windows 上用裸主机名（没有 scutil，也没有 `.local`）。
- Tailscale 可执行文件路径补上 Windows 默认安装位置，于是 Tailscale 的公信
  证书在 Windows 上也能接上（手机访问不再有警告）。
- 降级日志说清缺什么：「没找到 openssl」换成「缺 openssl 命令 / 缺 cryptography 库」，
  并直接给出修法 —— 原来那句话会让 Windows 用户去装一个装不上的东西。

### 说明
- **为什么不把 openssl.exe 打进包**：TLS 只需要「一对 PEM 文件 + 私钥」，
  `cryptography` 在进程内就能签出来，不需要命令行程序。打 openssl 进来是
  +6~8 MB，且 OpenSSL 官方**不发** Windows 二进制，第三方预编译包来源难辨 ——
  一个安全组件用来源不明的二进制，比升级一次依赖更危险。所以 spec 留了
  `vendor/openssl/` 钩子：真要放，你自己放，构建会带上；不靠它也能跑 HTTPS。

## [0.3.0] - 2026-11-23

### 新增
- **插件包 `.moshanplug`**（`moshan/pluginpack.py`）：一个 zip + 一份 `plugin.json`
  就是能装的插件。中台里三条路，都不用开终端 ——
  ① 把包（或整个插件文件夹）**拖到窗口任意位置**；② 「插件」面板里选文件 / 选文件夹；
  ③ 粘一个网址让中台**自己下**。装到 `_插件/<id>/`、登记到 `_工作台/plugins.json`，
  别的插件的行和用户数据目录一个字节都不动。
- **更新提示（本体 + 插件）**：`GET /api/updates` 一次给齐两边。工作台右下角常驻一颗
  「插件」胶囊，有新版就变成陶土色并报数（「2 项更新」）；点开第三页是明细 ——
  本体给「去更新」（跳 `/about` 走原来那套下载校验），插件给「更新」按钮，一点就换。
  `moshan/pluginui.py` 负责界面，**注入**到 `/dash`，`dash.html` 一个字节不改。
- 插件版本从 `_插件/<id>/plugin.json` 读；新版来源支持 `update.manifest`、
  GitHub Release、GitHub Pages 上的 `plugin.json`、raw 的 main/master —— **四路并发**，
  9 秒预算，谁先给出合法版本就用谁（串行时国内一条被墙的地址能挂 20 秒，
  四条串下来前端轮询直接超时）。

### 说明
- **解包不用 `extractall`**：逐条校验相对路径，`..`、绝对路径、符号链接一律拒绝；
  文件数 2 万、解压总量 512 MB、单文件 200 MB 封顶，先看 zip 声明的总大小再动手。
- **升级不清空目录**：新包覆盖同名文件，包里没有的旧文件留着（删错一次插件就起不来）。
  旧版整目录挪进 `_插件/.history/`，留最近 3 份可退回；**`.git` 会搬回来** ——
  git clone 装的插件（llmcost 就是）升级后还得能 `git pull`。
- **「删插件」不删数据**：数据目录（`data_default`）原样留着，卸载只挪程序。
- 没带启动脚本的包按 `entry` 自动生成 `run.command`：绝大多数插件就是「一个 py 起个服务」，
  让每个作者手写 .command 基本等于谁写谁忘 `chmod +x`。
- zip 里前两个 `</body>` 出现在内联 JS 的字符串里（启动占位页那两处），
  所以注入必须用**最后一个** `</body>` —— 用 `replace(…, 1)` 会把整段 UI 塞进 JS 字符串，
  浏览器报 `Invalid or unexpected token`，页面少一截还看着像「什么都没做」。

## [0.2.7] - 2026-11-23

**能说出「这台机器现在跑着什么模型」。** 进程卡一直能报「llama-server 吃了 20 GB」，
却答不出那 20 GB 是哪个模型 —— 而 20 GB 到底正常不正常，取决于跑的是什么。

### 新增
- `moshan/llmwatch.py`：本机大模型探测，三层取数逐层更准 ——
  进程名/命令行认服务（llama.cpp / Ollama / LM Studio / vLLM / SGLang /
  KoboldCpp / MLX-LM / GPT4All / Jan），命令行抠模型路径与上下文长度，
  再问端口自己（llama.cpp 的 `/props`、OpenAI 兼容的 `/v1/models`、
  Ollama 的 `/api/ps`）。模型名里顺带解析参数量和量化等级（`35B`、`Q4_K_M`）。
- `GET /api/llm`（`?fresh=1` 跳过 20 秒缓存），并进 `/api/overview` 的 `llm` 字段 ——
  页面跟着原来那 30 秒一轮刷，不多打一趟请求。
- 工作台新增「本机大模型」卡：模型名、参数量、量化、上下文、对外端口、
  整组进程内存、显存（服务自报）。模型名允许换行 —— 名字本身就是结论，
  截成「Qwen3.6-35B-A3B-Uncensore…」跟没说差不多。

### 说明
- **Ollama 不自己推理**：它把模型交给一个 `llama-server` 子进程，命令行里的路径是
  `~/.ollama/models/blobs/sha256-…`。不认出这层父子关系，界面上就会出现一条
  「模型名叫 sha256-a9d0a3e1…」的怪东西。所以按 blob 路径 + 父进程双重认，
  名字再用 `/api/ps` 换成人读的 `qwen3.5-4b-gguf:q4km`。
- **LM Studio 同理**：GUI 是个空壳，真正吃内存的是它拉起来的 `llama-server`。
  子进程归到父进程那一族，对外端口认 LM Studio 的 API 口（1234），
  不是子进程那个内部端口（实测 62253 这种）。
- **`/v1/models` 列的是「下载过的」，不是「加载着的」**：拿它当「在跑」，
  Ollama 那边会把整个模型库都列出来。所以 Ollama 只认 `/api/ps`。
- 服务活着但没加载模型是常态（Ollama 空闲 5 分钟就自动卸载），这种情况
  不列进模型表，改在卡片上写「服务在，没加载模型」—— 两件事不能混着说。
- 探测只打 127.0.0.1、超时 0.8 秒，且**必须绕开系统代理**：macOS 上开着
  `http_proxy` 时 urllib 会把本机请求也发给代理，结果是「探不到」而不是「没有」。

## [0.2.6] - 2026-11-23

**让 Windows 那台机器「有数据可看」：进程采样、巡检、解释器三处补齐。**

之前 EVO 上「本机工作」只有 TRAE 的逐轮记录、巡检页一片空 —— 不是那台机器没干活，
是三处代码默认自己跑在 macOS 上。这一版把这三处按平台分开。

### 新增
- `moshan/pyexe.py`：跨平台解释器解析。venv 路径按平台取
  （POSIX `bin/python` / Windows `Scripts\python.exe`），找不到时依次试
  当前解释器 → PATH → Windows `py` 启动器 → `%LOCALAPPDATA%\Programs\Python\Python3*`，
  并把**试过的每一条**带回界面。`ai_trends_runner` / `dedupe_runner` / `ocr_runner`
  三处写死 `.venv/bin/python` 的地方全部改走它。
- `hostwork._sample_procs_win()`：Windows 的进程采样。字段形状与 mac 完全一致，
  前端不用改；`_OWNER_PAT` 的分隔符改成 `[\\/]`（写死 `/` 在 Windows 上一条都匹配不上）。
  `psutil==7.2.2` 进依赖，`moshan.spec` 显式加 hiddenimports（函数内 import 抓不到）；
  缺它时返回 `why` 说明而不是空白。
- `moshan/selfcheck.py`：不依赖 cuntu 的**基线巡检**。8774（cuntu 的 syscheck）不在时
  `/api/inspect/run|state` 退回本机这一套：数据目录可写、磁盘、内存、运行时长、
  Python 可用、https 证书、本机端口、采集层命中、运行依赖、外网可达。
  报告写到 `10-系统记录/巡检/报告`（和 syscheck 同目录，读法不变），历史进 `历史.jsonl`。
- `docs/WINDOWS-PARITY.md`：mac / Windows 能力对照，哪些用系统自带、哪些要额外装、
  PowerToys 怎么定位（用户侧便利，不是程序依赖）。
- `heartbeat._run_once_win()`：心跳在 Windows 上改跑上面那套基线检查。原来那套
  （gateway / launchd / 锁 / node+brew / 微信会话 / App 监工）是 mac 的常驻结构，
  Windows 一项都不存在，照跑每轮都是「有检查项没过」—— 那是**配置不存在**，不是毛病。
  端口也改成探中台自己的（`CFG["port"]`），不是 gateway 的 18789。
- 工作台的「系统巡检」卡只留最近一条：`_collapse` 按状态分组，否则「各项正常」
  和上次的「有检查项没过」会并排出现，看的人以为现在还没好。

### 说明
- **`/api/overview` 在 Windows 上整份降级**：`hostwork._disk()` 调 `os.statvfs`，
  而 Windows 上它不是「抛 OSError」而是**根本没这个属性** —— 只 catch OSError 会漏，
  一漏整份 payload 走降级分支，资源卡、「谁在干活」、节奏账全变空表。
  改成没有 statvfs 时走 `shutil.disk_usage`。同一处的 `os.getloadavg()` 同理。
- 内存与开机时长在 Windows 上改由 `sysinfo` 提供（没有 `vm_stat` / `kern.boottime`）。
  不借这一步的话，内存会一直报 0 GB —— 不崩，但比空更糟：看着像真值。
- 文件名是 `selfcheck` 不是 `inspect`：`moshan/` 在 sys.path 上，叫 `inspect.py`
  会顶掉标准库，pydantic import 当场炸（`module 'inspect' has no attribute 'signature'`）。
- 「AI 新闻抓取」在 Windows 上一直报「找不到 Python」，根因是它按 macOS 的 venv 布局
  找解释器 —— 那台机器装着 Python 3.11，只是 `python` 不在 SYSTEM 的 PATH 上。

## [0.2.5] - 2026-11-23

**更新权交给主机：程序包由主机推过来，被更新的机器不需要 GitHub。**

`nucbox` 那台办公室电脑连不上 github.com（实测 8 秒超时），它也不该为了一次更新
去外网拉包。现在主机（mac / 虫巢中台）把程序包推过去，落盘、校验、切换、重启
一条链走完，不需要人到机器前 —— 和接入程序的 `self_update` 是同一个思路。

- 新增 `POST /api/update/push`：`{version, sha256, b64, restart}`，sha256 不符就
  **整包丢弃**，不落半成品；解包只认程序文件白名单，拒绝绝对路径与 `..`。
- 新增 `GET /api/update/status` 与 `POST /api/update/rollback`（回到上一版）。
- 新增 `POST /api/update/mode`：**由主机决定这台机器怎么更新**，把 `update.mode`
  钉成 `push` 之后 `/api/update/check` 直接返回「由主机推送」，不再去外网白等超时。
  配套 `host_push_update.py --set-mode push|github`（`--http` 直连 / `--fleet` 走虫巢）——
  nucbox 没有入站端口，只能借虫巢的 `proc_start` 在那台机器本地落地，人不用过去。
- **修**：TRAE 数据目录写死成 macOS 的 `~/Library/Application Support/Trae CN`，
  Windows 上「会话索引 / 项目快照 / 命令记录」三个源全被判定为「这台机器上没找到」。
  现在按平台取 `%APPDATA%\Trae CN`（结构一模一样：`User\globalStorage\state.vscdb`
  与 `ModularData\ai-agent\`），以 SYSTEM 身份跑时 `%APPDATA%` 指向 systemprofile
  会自动退到普通用户目录。
- 数据源自检补三行：`TRAE 会话记忆`（Windows 上唯一活着的逐轮来源）、`TRAE 窗口采样`、
  `TRAE 自述账`。以前这三条不列，页面显示「TRAE 全线没数据」而逐轮其实是好的 ——
  少列一行会误导一整轮排查。
- 新增 `scripts/host_push_update.py`：两种送法 —— `--http` 直连；
  `--fleet <device>` 走虫巢（分片 put + 目标机回环 POST），入站全封的机器也能更新。
- **修**：交付版「逐轮工作」永远是空。`hostwork._rounds_parser()` 只认
  `~/Documents/cuntu/dash.py`，安装版没有 cuntu 就直接返回 None —— 作者本机
  永远复现不了。现在回退到包内 `dash` 模块。
- 构建脚本加固（两条都是真踩过的）：`build.ps1` 强制 `Set-Location` 到仓库根
  （远端后台构建的 CWD 是 `C:\Windows\System32`，PyInstaller 会拒绝运行）；
  `.ps1`/`.cmd` 一律纯 ASCII —— PS 5.1 把无 BOM 的 UTF-8 当 GBK 读，末尾汉字会
  吞掉换行、把下一行并进注释，命令静默失效。`scripts/check.sh` 新增第 8 节专查。
- 发版流程：Release 正文改由 CI 从本文件的对应版本段落抽出，不再手工往 GitHub 上贴
  （手贴一定会漏，且漏了没人发现）。`docs/ARCHITECTURE.md` 的「更新流程」按两条路重写，
  跨平台差异表补上 TRAE 数据目录、侧栏 OCR、Cherry 账本在 Windows 上的实际状态。
- `scripts/check.sh` 的解释器可用 `MOSHAN_PY` 指定；缺 fastapi / certifi 时明确说
  「这是环境问题」，不再伪装成代码失败。

## [0.2.4] - 2026-11-23

**把「本机工作」的采集层收进交付包：别人机器上的 Moshan 不再是空壳。**

以前 `hostwork` / `trae_work` 只躺在作者本机的 `cuntu/` 里，交付版没带，于是每台
新装的机器都显示「本机工作模块没装上」，只能退到 `_CLIENTS` 那份降级账 ——
而且作者本机永远复现不了这个问题。

### 新增
- `hostwork.py`（172 KB）、`trae_work.py`、`cherry_usage.py`、`billing.py` 随包分发。
  `hostwork` 负责本机工作采集（会话记忆 / 进程 / 用量 / 计费对账），`trae_work` 是
  它的 TRAE 取数层，另外两个分别是 Cherry Studio 用量账本与供应商账单
- `hostwork_ui.html` 内置：`/hostwork` 详情页不再依赖外挂目录
- `/hostwork` 的查找顺序改成「先 cuntu、再包内」：开发机行为不变，打包版不再 404

### 修复
- `moshan/paths.py` 补 `ensure()` 与 `CONF_FILE`。`hostwork` / `billing` 在**模块级**
  就调用 `paths.ensure(data_root() / "_本机工作")`，缺这个函数是导入当场 AttributeError，
  不是运行期降级 —— 整个模块直接不可用
- `packages/moshan.spec` 的 `datas` 增加 `hostwork_ui.html`；`scripts/sync-from-dev.sh`
  的 `EXTERNAL` 增加这五个文件，以后同步不会再漏

### 说明
- `cherry_usage` 的账本路径仍是 macOS 的 `~/Library/Application Support/CherryStudio`，
  Windows 上取不到就采空（不报错）；要支持 Windows 得另接 `%APPDATA%`，这一版没做
- `trae_work` 里的窗口标题采样走 CoreGraphics，非 macOS 会在 `try` 里失败并跳过，
  不影响其余采集
- 采集线程在 `server` 里启动，异常只写日志（`hostwork_thread_error`），不会拖死中台

## [0.2.3] - 2026-11-23

**让 Trae 自己把「干了什么 + 花了多少」报进中台，装完新版本不用手点。**

### 新增
- `Trae 自述通道`：`POST /api/trae/note` 现在收用量（`model` / `in` / `out` / `cached` /
  `calls`），落 `_本机工作/TRAE自述.jsonl`；工作台把这份自述折成逐轮记录，
  token 就显示在那一行上，跟会话记忆读出来的条目并列（两者 id 不冲突）
- **升级后自动投放说明书**：启动时若 `~/.trae-cn` 存在且投放版本与当前版本不一致，
  就把 `pocket-bridge` 技能写进 Trae 的技能目录，日志会留一行
  `已更新 TRAE 自述说明书 x.y.z → <路径>`。没装 Trae 的机器直接跳过，不凭空造目录
- `scripts/check.sh` 增加第 7 节：断言说明书里写的是 `/api/trae/note`（不是总线那条
  `/api/agent/event`）、带用量字段、`auto_install` 存在、工作台确实读自述文件

### 修复
- **干净机器上 `/api/trae/note` 直接 503**：接口依赖 `cuntu/trae_work.py` 取路径，
  那模块没随包分发，于是新装的机器一条自述都收不进来。现在模块不在就自己拼
  `<数据根>/_本机工作/TRAE自述.jsonl`，写和读指向同一个位置
- 说明书原来让人把账报给 `/api/agent/event`（agent 总线）和 `/api/agent/usage`，
  而工作台 TRAE 卡读的是自述文件 —— **报得再勤，卡片也是空的**。现在工作面明确指向
  `/api/trae/note`，总线那条降级为「给别的 agent 看」的可选项

### 说明
- Trae 的命令 / 进程 / 会话记忆那三路自动采集仍依赖开发目录里的 `trae_work` /
  `hostwork`，这两个模块没进仓库。这一版保证的是「自述」这条**不依赖 Trae 本地库**
  的通道：Trae 主动 POST，换机器、换 Trae 版本都不会断
- 用量估不出来就填 0。说明书里写死了这条，卡片宁可显示 0 也不显示编出来的数字

## [0.2.2] - 2026-11-23

**修 Windows 版首次启动必崩：双击图标没反应，界面不出来。**

### 修复
- **Windows 首次启动崩溃**：`tls._generate()` 里 `tempfile.mkstemp()` 返回的是
  「已打开的 fd + 路径」，代码只取了路径，fd 一直没关。证书生成完在 `finally` 里
  `unlink()` 那个临时 `.cnf` 时，POSIX 允许删除仍被打开的文件（所以 macOS 一直正常），
  Windows 直接 `PermissionError: [WinError 32]`；异常从 `finally` 穿出去打死
  `server.main()`，PyInstaller 再弹一个 traceback 对话框把进程挂住 —— 用户看到的
  就是「双击没反应」。现在 fd 立刻关闭，`unlink` 失败也只忽略
- **证书生不出来不再拖死启动**：`tls.ensure()` 整体兜一层，`openssl` 缺失、调用超时、
  权限问题一律退化成明文 HTTP 继续启动。证书只决定手机端能不能用 https，
  不该决定程序能不能打开
- **TLS 建不起来时本机端口根本不监听**：本机明文端口 `local_port` 的绑定原先写在
  `if scheme == "https"` 里，一旦退化成 http 就一个都不绑；而启动器打开的正是
  `127.0.0.1:{local_port}`，等于把用户领到一个没人监听的端口。现在无条件绑定（仅 loopback）

### 说明
- 这一版的问题是 CI 头一回真正「把安装包跑起来」才发现的：构建日志全绿、
  macOS 侧冒烟也过，只有 Windows 会崩。**构建成功 ≠ 装完能用** —— 上一版缺 CA
  证书是同一个教训，所以这轮没有靠读代码下结论，而是把安装包拉起来打接口
- 冒烟判据改成「以程序自己打印出来的本机访问地址为准」，不再写死某个端口；
  失败时自动 dump 启动日志、`config.json`、`netstat` 监听项与进程退出码
- **0.2.0 的用户直接下 0.2.2**：0.2.0 的安装版缺 CA 证书（任何 https 都会失败，
  包括「检查更新」本身），Windows 侧还叠加了上面这个启动崩溃，自动升级这条路走不通

## [0.2.1] - 2026-11-23

**这个版本修的是 0.2.0 的一个致命问题：安装版里所有 https 请求都会失败。**
装过 0.2.0 的人没法靠「检查更新」升上来（坏的正是这条链路本身），需要手动重新下载。

> 注：0.2.1 的 tag 打出去了，但构建阶段 Windows 冒烟没过，Release 没有发布出去 ——
> 请直接使用 0.2.2（Windows 启动崩溃也是在这一版后续修掉的）。

### 新增
- `netca.py`：启动最早期把 `SSL_CERT_FILE` 指向随包分发的 certifi，给整个进程
  建立可信的 HTTPS 信任链。打包入口 `launcher.py` 与 `server.py` 都会调用，
  两种启动方式行为一致
- `scripts/check.sh` 增加「网络信任链」一节：断言 certifi 已安装、`netca.install()`
  确实选中它、`ssl.create_default_context()` 里装进了根证书（离线可跑，不需要发请求）
- 下载元数据增加 `verified` 字段。GitHub 没回 `digest` 时界面不再谎称「已校验通过」，
  改为如实提示「未做完整性校验」

### 修复
- **安装版 https 全线失效**：PyInstaller 打出来的应用用自己的 OpenSSL，默认 CA 路径
  （macOS 上是 Homebrew 那种 `/usr/local/etc/openssl@3/cert.pem`）在别人机器上不存在，
  于是 `urllib.request.urlopen()` 一律抛
  `CERTIFICATE_VERIFY_FAILED: unable to get local issuer certificate`。
  影响的不只是「检查更新」——抓网页、存图、装插件（共 12 处 `urlopen`）全都用不了。
  **源码态完全复现不出来**：本机有 `/etc/ssl/cert.pem`、site-packages 里也有 certifi，
  所以本地自检、CI、手工验证全是绿的。0.2.0 就是这样发出去的
- `requirements.txt` 显式加 `certifi`，`packages/moshan.spec` 显式收它的数据文件。
  以前它是 httpx 的间接依赖，代码里没人 import 它，PyInstaller 就不会带进包
- `version.version_tuple()` 会剥掉 `v` 前缀。Git tag 与 `release.json` 都带 `v`，
  1.x 版本上会被解析错（`v1.2.3` → `0.2.3`），此前只是因为 0.x 才侥幸没出事
- CI 生成的 `release.json` 里 `version` 改为不带 `v`；原先把 tag 名原样写进去，
  谁拿它跟 `version.VERSION` 比较都会出岔子

### 说明
- Release 附件只有 dmg / exe / `release.json`，**不再单独发 `.sha256` 文件**。
  校验值在 `release.json` 和 GitHub API 每个资产的 `digest` 字段里（两者同源同值，
  0.2.0 的 dmg 实测三方一致），更新器读的是后者。单独一个 `.sha256` 附件只是噪音

## [0.2.0] - 2026-11-23

### 新增
- **关于 / 更新页 `/about`**：显示版本、构建、平台、三层数据目录，并能检查更新、
  下载（带进度）、打开安装包。首页右下角注入一个小入口，有新版本时自动变成
  「新版本 x.y.z」；后台每 6 小时静默查一次，等于把更新推到你眼前
- 更新器命令行入口：`--check` / `--download` / `--apply`，发版后可在终端自测更新链路
- `sysinfo.py`：内存 / CPU 核数 / 负载 / 开机时长 / 进程数的跨平台采集。装了 `psutil`
  就用它，没装则分平台兜底（macOS 调 `sysctl` + `vm_stat`、Linux 读 `/proc/meminfo`、
  Windows 用 `ctypes` 调 `GlobalMemoryStatusEx`）；量不到就返回 `null`，不编数字
- `GET /api/sys`：轻量机器状态，供需要高频轮询的卡片使用；与偏重的 `/api/status`
  分开，避免为了五个数把整包账算一遍
- `POST /api/trae/note`：Agent 收尾时自报「这一轮干了什么」，同一任务 5 分钟内只记一次；
  没装 cuntu 扩展时接口返回 503 并说明原因，不影响其他功能

### 修复
- **交付版缺 `textfmt.py`，新用户装完打不开**。`article.py` 在模块顶层 `import textfmt`，
  而这个文件只在作者的 cuntu 扩展目录里，仓库里没有；开发机上有 cuntu 所以本机永远正常，
  CI 一跑导入冒烟就红。现已把 `textfmt.py`（纯标准库，17KB）纳入仓库，并在同步脚本里
  加了 `EXTERNAL` 机制从 `cuntu/` 取这类文件，避免下次再漏
- **`check.sh` 的导入冒烟只抽查 8 个模块，且用真实 HOME 跑**，所以「本地全绿、CI 全红」
  成了常态。现在改成导入 `moshan/` 下全部 43 个模块，并把 `HOME` 指向空目录 ——
  开发机上的 `~/Documents/cuntu` 再也掩盖不了缺文件
- **交付版会把数据写进别人的 `~/Documents/AI项目`**：`article` / `linkbox` / `heartbeat` /
  `metrics` / `pending` / `ocr_runner` 各自写死了开发机的目录结构，其中三处在启动时
  `mkdir`，等于在用户的 `Documents` 里凭空造出一个中文目录。新增 `paths.data_root()`
  作为唯一出口，分发版即资料库；`MOSHAN_CUNTU` 统一到这几个模块，Windows 的
  cuntu 解释器路径也改对了（`Scripts/python.exe`）
- **`metrics.py` 读的是程序目录里的 `config.json`**，打包后那个位置没有配置，
  于是永远退化到写死路径。改用 `paths.config_file()`
- **`updater._asset_pattern` 的正则写错**：`(?i)` 只能出现在表达式开头，写成
  `a|(?i)b` 会抛 `re.error: global flags not at the start of the expression`。
  结果「检查更新」对所有人都是 500。改用 `re.IGNORECASE`，并在 `scripts/check.sh`
  里加了一条断言（用 CI 真实产出的资产名做匹配），防止再犯
- 更新源的默认仓库地址写错（`pamphile0117/moshan` → `blackinkvc/moshan`），
  会导致「检查更新」直接 404
- `CUNTU_DIR` 从写死的家目录改为可被 `MOSHAN_CUNTU` 覆盖，与 `ocr_runner.py` 一致；
  分发版没有这个扩展目录时静默跳过

### 文档
- README 重写：能力清单（标出哪些功能需要模型）、安装与首次引导、数据三层与
  「更新不丢东西」的原因、更新流程、目录结构、构建与发版、Agent 接口、安全模型、
  已知限制
- README 版式：居中标题 + 手绘「山」字标 + 徽章行 + 首屏截图；界面、引导、更新
  三处各配一张真实截图，手机端单独一张竖图
- 新增 `docs/images/`（5 张界面截图 + `logo.svg`），CONTRIBUTING 补「截图怎么更新」，
  写明截图必须用临时数据目录，不许拿真实资料库拍

## [0.1.0] - 2026-11-22

首个可分发的版本。

### 新增
- 桌面启动器：自动选端口（冲突顺延）、建目录、唤起浏览器
- 三层目录约定：程序 / 状态 / 资料库物理分离，覆盖安装不丢数据与设置
- 新手引导页 `/welcome`：环境检查、Ollama 探测、一键拉取模型、进度实时回显
- 更新器：查询 GitHub Release、下载、sha256 校验（不匹配即丢弃）、唤起安装包
- macOS 打包（PyInstaller onedir + .app + ad-hoc 签名 + dmg）
- Windows 打包（PyInstaller + Inno Setup 单文件安装程序）
- GitHub Actions：推 `v*` 标签自动双平台构建并发布 Release，含 `release.json` 与校验值
- CI：语法检查、隔离数据目录的导入冒烟测试、密钥与硬编码路径扫描
- Agent 接入 `/api/agent/link/*`：认人（scan）→ 装说明书（install，写进 Trae/Cursor/
  Claude/WorkBuddy 的技能目录）→ 收回报（report）→ 按回执导入（import），
  把别的 Agent 的会话记忆导进中台自报账

### 变更
- `server.py` 的启动逻辑抽成 `main()`，供启动器在同一进程内调用
- 配置默认路径从写死的 `/Users/pamphile/...` 改为跨平台取值，老用户仍可继承
  同目录下 `feishu_bridge/config.json` 的网关密钥
- 桌面形态下 stdout/stderr 重定向到 `状态目录/logs/moshan.log`
- 服务方阵按平台过滤：`gallery / reader / media / inspect / llmcost / follow /
  review / gateway / qdrant` 只在 POSIX 上显示，Windows 上不再一开就是一排红点
- 工作流水里导入/补记的账最多占 24 格，不会被近期会话挤出列表
- Agent 接入在 Windows 上会扫描 `C:\Users\*` 找真正的家目录（以服务身份运行时
  `Path.home()` 会指到 `systemprofile`，那里面没有任何 Agent 数据），并给扫描结果
  加 20 秒缓存，避免反复开关面板把几百 MB 目录走一遍

### 已知限制
- macOS 安装包未做 Apple 公证，首次打开需右键「打开」
- Windows 上缩略图功能降级为显示原图
- 无系统托盘，退出需在 Dock 上 Cmd+Q（macOS）或结束进程（Windows）
- 模型依赖 Ollama；自定义 OpenAI 兼容端点需手工填配置
