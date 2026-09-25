# QBot

QBot 是一个参与 QQ 群聊的 AI 成员。被 @、被叫到名字或被引用时它会回复；它按群记住
群友和往事；它能看图、能听语音；花费受你设定的上限约束。

[English](README.md)

## 特性

- **只在被叫到时说话。** @、整词出现的昵称、引用机器人自己的消息，三者之一触发回复。
  其余消息只读取、存档，不出声。
- **结构化记忆。** 每晚由模型从聊天记录归纳关于成员和群的事实，由代码依据原文引语
  校验，带证据、置信度和有效期存储。错误条目可按编号删除。
- **区分账号、称呼与人。** 两个账号可以合并为一个人；提示词里每个人都带成员编号，
  同名成员不会混淆。
- **QQ 原生的回复形式。** 模型通过工具调用发出回复，自行决定 @ 谁、回复哪条消息，
  或者发一条普通消息。
- **图片与语音。** 每张图片在到达时写一行描述并以文本存档，回复模型按需取回原图查看。
  语音在到达时于 CPU 上本地转写，不产生费用。
- **工具。** 回复模型可以搜索网页、用布尔表达式检索本群存档、按语义召回往事、读取网页
  正文、查看原图。
- **钱是唯一限制。** 日花费上限、单次回复上限、搜索月额度。没有 token 预算，也没有
  调用次数配额。
- **一切按群。** 人设、群知识、记忆、屏蔽名单与静音开关都以群为作用域。
- **直接使用。** 群成员无需先注册或同意协议，被叫到即可得到回复并使用成员指令。
- **群内运维控制台。** 查看和修正记忆、屏蔽或静音、查看用量、捕获模型调用以便排查。

## 工作原理

```text
QQ  <->  NapCat (OneBot v11)  <-- 反向 WebSocket -->  bot  <-- asyncpg -->  PostgreSQL + pgvector
```

三个容器。NapCat 是 QQ 协议端，通过反向 WebSocket 连接到 bot；bot 是一个 NoneBot2
应用；PostgreSQL 保存存档、记忆模型、作业队列和费用账本。

bot 只认五种能力：文本、视觉、语音识别、向量化、网页搜索。每种能力由哪个服务商、
哪个模型、哪个凭证提供，在 `config/settings.yaml` 中声明。默认配置使用 DeepSeek 提供
文本与视觉，进程内的 sherpa-onnx + SenseVoice 做语音识别，阿里云 DashScope 做向量化，
Tavily 做搜索。文本与视觉后端使用 Responses API；工具调用、工具结果和推理续接由每条回复任务在本地维护，不依赖服务商侧会话。新增服务商只需写一个子类并注册。

完整说明见 [docs/architecture.md](docs/architecture.md)。

## 环境要求

- Docker 与 Docker Compose
- 一个供机器人使用的 QQ 账号（强烈建议专用账号）
- 配置中所用服务商的 API 密钥
- 如需在本地运行测试或评测脚本，需要 Python 3.12

第三方 QQ 协议端违反腾讯的服务条款，账号可能被封禁。请使用可以承受损失的账号。

## 快速开始

```bash
git clone <本仓库> qbot
cd qbot

cp .env.example .env
cp config/settings.yaml.example config/settings.yaml
cp config/personas/default.yaml.example config/personas/default.yaml
```

编辑这三个文件：

- `.env`：API 密钥、PostgreSQL 密码、NapCat 登录的 QQ 号。
- `config/settings.yaml`：拥有者账号、机器人昵称、各服务商。
- `config/personas/default.yaml`：机器人的名字和性格。

然后下载语音识别模型、启动容器、登录 NapCat，并在上线前检查各服务商：

```bash
bash scripts/fetch_asr_model.sh          # 一次即可，约 250 MB，存入 models/

docker compose up -d postgres napcat
docker compose logs -f napcat             # 扫码，或打开 http://127.0.0.1:6099

# 首次登录后，把 NapCat 指向 bot（见 docs/operations.md）：
#   将 napcat/onebot11.json.template 合并进 data/napcat/config/onebot11_<QQ号>.json
docker compose restart napcat

docker compose build bot
docker compose run --rm bot python scripts/preflight.py   # 每个服务商各调用一次
docker compose up -d bot
```

群从第一条消息起即被服务。没有白名单；新群会出现在每日报告里。

两个真实配置文件都被 git 忽略，因为它们写有真实账号；仓库里只有 `.example` 模板。

## 配置

行为在 `config/`，凭证在 `.env`，运行状态在数据库。

| 文件 | 用途 |
| --- | --- |
| `.env` | Docker Compose 读取的凭证与基础设施参数 |
| `config/settings.yaml` | 全局设置：拥有者、触发、服务商、预算、提示词窗口、记忆、定时任务 |
| `config/personas/default.yaml` | 默认人设：名字、系统提示、群知识 |
| `config/personas/group_<群号>.yaml` | 单个群的人设：名字、提示词补充与固定群背景 |
| `config/predicates.yaml` | 可以记录关于一个人的哪些内容 |
| `config/prompts/prompts.yaml` | 模型读到的全部指令模板 |

完整配置会在启动时统一校验。设置、人设、提示词或谓词表修改后，需要重启进程
才能生效。参考见 [docs/configuration.md](docs/configuration.md)。

## 指令

指令对所有调用者只有一种含义，身份只决定是否有权执行。涉及人的指令默认作用于精确
账号，只有显式 `--all` 才作用于当前关联账号集合。

| 指令 | 用途 |
| --- | --- |
| `/help` | 显示所有人共用的指令目录与权限标签 |
| `/who`、`/note`、`/alias`、`/forget` | 查看和修正精确账号或显式关联集合记录 |
| `/link`、`/unlink` | 双端确认自己的关联账号，或剥离当前精确账号 |
| `/card`、`/stats`、`/top` | 本群记录与用量 |
| `/members`、`/merge`、`/split` | owner 查看目录和修复身份关系 |
| `/block`、`/mute` | owner 管理回复屏蔽与群静音 |
| `/debug`、`/log` | 维护 |

用法与权限见 [docs/commands.md](docs/commands.md)。

## 运维

部署、备份、恢复、回滚、数据库结构变更、定时任务、每日报告、排查与行为评测脚本见
[docs/operations.md](docs/operations.md)。

## 开发

测试套件跑在一个一次性的 PostgreSQL 上，不需要连接 QQ：

```bash
python -m venv .venv
.venv/bin/pip install -r requirements.txt -r requirements-dev.txt

docker run -d --name qbot-pgtest \
  -e POSTGRES_DB=qbot_test -e POSTGRES_USER=qbot_test -e POSTGRES_PASSWORD=testpw \
  -p 15432:5432 \
  -v "$PWD/sql/init.sql:/docker-entrypoint-initdb.d/01-init.sql:ro" \
  -v "$PWD/tests/fixtures/test_db_marker.sql:/docker-entrypoint-initdb.d/02-test-marker.sql:ro" \
  pgvector/pgvector:0.8.5-pg17

.venv/bin/python tests/run_all.py        # 全部套件，然后 ruff
```

约定见 [CONTRIBUTING.md](CONTRIBUTING.md)，各套件的覆盖范围见
[tests/README.md](tests/README.md)。

## 目录结构

| 路径 | 内容 |
| --- | --- |
| `bot.py` | 入口；提供 OneBot 反向 WebSocket |
| `qqbot/plugin.py` | 薄 NoneBot 生命周期与事件适配层 |
| `qqbot/runtime.py` | 进程级装配根与有序生命周期所有权 |
| `qqbot/scheduled.py` | 不依赖框架的夜间与报告任务主体 |
| `qqbot/settings.py` | 启动配置模型与人设合并 |
| `qqbot/gateway/` | OneBot 归一化与数据库 append-once 准入 |
| `qqbot/core/` | 入站路由、指令、投递、触发、提示词、工具、媒体和预算 |
| `qqbot/domain/` | 类型化入站事件与记忆模型：身份、称呼、事实、情景、证据 |
| `qqbot/repositories/` | 记忆模型的数据库访问 |
| `qqbot/services/` | 抽取、校验、归并、成员目录 |
| `qqbot/workers/` | 后台记忆工作器 |
| `qqbot/providers/` | 服务商抽象，每个后端一个模块 |
| `qqbot/plugins/` | 定时任务的薄注册适配层 |
| `qqbot/db/` | 连接池、结构检查、存档与账本访问 |
| `config/` | 配置模板、提示词与谓词表 |
| `sql/` | 数据库结构与结构变更记录 |
| `scripts/` | 部署、上线检查、模型下载、评测 |
| `tests/` | 测试套件 |
| `docs/` | 架构、配置、指令、运维 |

## 许可证

[MIT](LICENSE)
