# katago-webui
基于 **KataGo** 的围棋 AI Web 界面：后端使用 Rust(Axum)，前端为极简静态页（HTML/JS）。当前支持人机实时对局（MVP），按 sid 并发≤3，30 分钟无活动过期回收。

## 功能（MVP）
- 实时对局：人类 vs AI（GTP 封装，或占位应手）
- 难度星级：5 档（1★–5★，服务端映射为访问/时间/温度参数；占位待接入）
- 会话并发：同一 sid 同时最多 3 局，第 4 局 429
- 自动回收：30 分钟无活动自动关闭对局
- 仅 HTTP：/api/game/* 与 /api/engine/status；静态托管前端

## 目录结构
```
backend/        # Rust + Axum
frontend/
  public/
    index.html # 极简对局页
```

## 环境变量（.env）
- 仅读取 `backend/.env`（不读取仓库根 `.env`）。
```
PORT=8080
CONCURRENCY_PER_SID=3
GAME_TTL_MINUTES=30
ENGINE_PATH=/home/swartz/WorkSpace/katago-webui/katago-cuda/katago
MODEL_PATH=/home/swartz/WorkSpace/katago-webui/katago-cuda/kata1-b18.bin.gz
GTP_CONFIG_PATH=/home/swartz/WorkSpace/katago-webui/katago-cuda/default_gtp.cfg
no_proxy=localhost,127.0.0.1,::1
NO_PROXY=localhost,127.0.0.1,::1
```
> 生效优先级：进程真实环境变量 > `backend/.env`。代码启动时自动加载；生产建议使用系统环境变量或 systemd `EnvironmentFile`。

## 运行
1) 安装 Rust（若未安装）
```bash
curl -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```
2) 启动后端
```bash
cd backend
RUST_LOG=info cargo run
```
3) 打开前端
- 浏览器访问：`http://localhost:8080/`
- 点击“新开对局”，在棋盘点击即可触发 `/api/game/play`

## 自启动安装（systemd）
提供一键安装/卸载脚本，适用于大多数 Linux（基于 systemd）。

前置：确保已安装 Rust，并在 `backend/.env` 配置好引擎路径与端口。

安装并开机自启（首次安装会自动构建并立即启动）：
```bash
sudo ./scripts/install-service.sh
```

常用命令：
```bash
# 查看服务状态
systemctl status katago-webui.service

# 跟随日志
journalctl -u katago-webui.service -f

# 停止 / 启动 / 重启
sudo systemctl stop katago-webui.service
sudo systemctl start katago-webui.service
sudo systemctl restart katago-webui.service
```

卸载自启动：
```bash
sudo ./scripts/uninstall-service.sh
```

说明：
- 单元文件位置：`/etc/systemd/system/katago-webui.service`
- 以当前 sudo 调用者身份运行（自动识别 `$SUDO_USER`）
- 读取环境：仅 `backend/.env`

## 配置 KataGo（可选）
- 路径在 `.env` 中配置（未配置则使用占位应手）。
- 自检：
```bash
echo -e "version\nquit\n" | "$ENGINE_PATH" gtp -model "$MODEL_PATH" -config "$GTP_CONFIG_PATH"
```

## HTTP API（片段）
- `POST /api/game/new` → 201 `{ gameId, expiresAt, activeGames }`（超限 429）
- `POST /api/game/play` → 200 `{ engineMove, captures, end }`（占位或真引擎）
- `POST /api/game/heartbeat` → 204（保持活跃）
- `POST /api/game/close` → 204（释放资源）
- `GET /api/engine/status` → 200（在线/当前 sid 活动局数/并发上限/运行时长）
- `GET /healthz|/readyz` → 200（存活/就绪）

## 注意
- 代理导致 502：调用本机请使用 `--noproxy localhost` 或设置 `NO_PROXY`
- 端口占用：设置 `PORT` 改端口
- 安全：`gameId` 绑定当前 sid，跨会话访问会被拒绝（后续完善）
- 心跳与清理：前端默认每 15 秒发送 `/api/game/heartbeat`；后端每 60 秒清理超时对局，超时时长由 `GAME_TTL_MINUTES` 控制，无需单独配置心跳间隔。
 - Komi：在 Chinese 规则下默认设为 7.5；其他规则沿用传入值。

## 路线图
- 难度分层：映射到 KataGo 覆盖参数（maxVisits/maxTime/温度/选点温度/认输策略）
- 悔棋/认输完善、落子合法性与坐标/规则细化
- 进程关停与异常恢复更健壮（TERM/KILL 时序）
