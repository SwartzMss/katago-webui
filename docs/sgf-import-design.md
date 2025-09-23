# SGF 导入显示功能设计（精简版 v11）

## 背景
页面目前仅支持实时对弈。我们计划增加“导入 SGF → 展示棋盘局面”的能力，供用户快速查看当前棋盘，并在需要时将任意手数保存为习题。保存习题时必须指定标签，以便在习题库中进行分类管理；默认提供“入门”“进阶”两个难度分类标签。导入操作不会自动写入习题库，KataGo 分析用于辅助判断局面。

## 功能范围（MVP）
- **导入来源**：
  1. 本地文件：用户通过 `<input type="file" accept=".sgf">` 选择 SGF。
  2. 网络文件：用户输入远程 SGF URL（限定 https，可选白名单），后端拉取。
- 后端解析 SGF，提取棋局描述：
  - 元信息：双方、规则、Komi、结果、备注等。
  - 主线着法或静态布局，用于恢复棋盘状态。SGF 可能只描述局面（`AB/AW`），也可能包含整盘落子序列；解析时保留全量着法，以便用户自由选择手数并进行分析/保存。
- 前端展示：
  - 默认渲染 SGF 主线的**最终局面**，导入后直接显示当前棋盘。
  - 展示元信息（对弈双方、结果、Komi 等）。
  - 平时无需回放控件，但在保存或分析时需允许用户挑选手数，因此提供“手数选择器”（滚动条或步进按钮），仅在相关弹窗/面板中出现。
- 会话级缓存：解析结果保存在内存 map（`reviewId` → TTL 30 分钟）。
- 用户手动保存：前端提供“保存到习题库”按钮，弹出对话框让用户选择手数、填写标题、**选择标签（必填，可多选）**，默认标签列表包含“入门”“进阶”，同时允许新建标签（如“提升”“死活”）。可多次基于同一个 SGF 保存不同手数的习题。
- KataGo 分析：前端提供“AI 分析”按钮，将当前选定手数对应的局面交给 KataGo，返回建议点、胜率等辅助信息。

## 用户流程
1. 用户点击“导入 SGF”，选择本地文件或输入远程 URL。
2. 前端调用 `/api/review/import`：
   - 文件上传：`multipart/form-data`（字段 `sgf_file`）。
   - 远程地址：JSON `{ "sourceUrl": "https://..." }` 或 multipart 字段 `source_url`。
3. 后端解析成功后返回：
   ```json
   {
     "reviewId": "r-...",
     "boardSize": 19,
     "komi": 7.5,
     "meta": { "black": "Black Player", "white": "White Player", "result": "W+1.5" },
     "finalStones": {
       "black": ["pd", "dd", ...],
       "white": ["qp", "oq", ...]
     },
     "moves": [
       { "index": 1, "color": "B", "coord": "pd" },
       { "index": 2, "color": "W", "coord": "dq" },
       ...
     ]
   }
   ```
4. 前端默认根据 `finalStones` 渲染棋盘；`moves` 存入状态，供保存或分析使用。
5. 保存为习题：
   - 点击“保存到习题库”，弹窗展示手数选择器（滑块/数字输入 + 上一手/下一手按钮），实时预览对应局面。
   - 仅选择分类标签（必选，且仅限“入门”或“进阶”其一）。无需填写标题、来源等额外信息。
   - 选择答案来源：
     1. **沿用 SGF 主线**：自动截取 `moveIndex` 之后的主序列若干步，作为标准答案。
     2. **采用 KataGo 建议**：调用分析接口获取首选 PV，保存前可预览并选定长度。
     3. **手动录入**：在弹窗内逐步点击棋盘录入解答，支持多个候选解（正确/备选）。
   - 可多次选择不同 `moveIndex` 并分别保存。
6. AI 分析：
   - 在相同弹窗或独立“AI 分析”按钮中选择手数。
   - 前端调用 `/api/review/analyze`，展示返回的建议点、胜率、目差等数据。
7. `/api/exercise/save` 或 `/api/review/analyze` 返回结果后更新 UI。若未保存，解析结果在 TTL 到期后自动清理。

## 后端实现要点
### `/api/review/import`
- 支持 multipart 与 JSON：
  1. 读取本地文件或根据 `sourceUrl` 下载 SGF（设置超时、大小限制、https 校验）。
  2. 使用 SGF 解析库提取根属性、主线、静态布局。
  3. 构建完整落子序列 `moves` 及最终局面：
     - 根据 `AB/AW` 初始化棋盘（如果存在）。
     - 依次模拟所有着法，记录着手信息；前端可按 `moves` 重放。
  4. 将包含 `moves` 的解析结果存入 `review_store: DashMap<String, ReviewState>`。
  5. 返回 JSON 响应。

```rust
struct ReviewState {
    sid: String,
    created_at: i64,
    last_active_at: i64,
    board_size: u32,
    komi: f32,
    meta: GameMeta,
    moves: Vec<MoveNode>,       // 全量主线着法
    initial_setup: InitialSetup,// AB/AW/AE 等静态信息
    source: ReviewSource,       // LocalUpload | RemoteUrl(String)
    raw_sgf: String,
    analysis_cache: HashMap<u32, KataAnalysis>,
    engine: Option<Arc<GtpEngine>>, // 懒加载 KataGo
}
```

### `/api/exercise/save`
- **请求体**：`{ reviewId, moveIndex, category }`
  - `category` 只能为 `"beginner"`（入门）或 `"advanced"`（进阶）。
- **流程**：
  - 校验 `reviewId` 属于当前 sid，并存在于 `review_store`。
  - 校验 `category` 是否在允许列表（`beginner` / `advanced`）。
  - 根据 `moveIndex` 计算保存局面：重新应用 `initial_setup` 并重放 `moves[0..moveIndex]`。
  - 处理答案：
    - 若前端指定 `answer.source = "sgf_mainline"`，截取 `moves[moveIndex+1..moveIndex+answerLength]` 作为 `answer.primary`。
    - 若 `answer.source = "katago"`，按照提交的 KataGo PV 写入（数组形式）。
    - 若 `answer.source = "manual"`，信任前端上传的落子序列，后台仅做合法性校验（坐标格式、轮转）。
  - 计算 `raw_sgf` SHA256，生成 `exerciseId = ex-{hash_prefix}-{moveIndex}`；支持同一 SGF 多手数多次保存。
- 写入目录 `backend/data/exercises/{exerciseId}/`：
    - `payload.json`：保存关键数据（`category`、`moveIndex`、`boardSize`、`komi`、`initialSetup`、`questionStones`、`answerSequences`、`createdAt` 等），练习页读取即能恢复题面；如需备份，可附加 `rawSgf`。
  - 若同一 `exerciseId` 已存在，可依据需求覆盖或提示重复。
- **响应**：`{ exerciseId: "ex-..." }`

### `/api/review/analyze`
- 同先前设计：根据 `reviewId` + `moveIndex` 构造局面调用 KataGo 分析，结果缓存并返回建议点、胜率、PV 等。

### `/api/review/close`（可选）
- 用户退出时主动释放 `reviewId`；否则由定时清理任务按 TTL 移除。清理时若 `engine` 存在，调用 `quit()` 关闭 KataGo。

## 前端实现要点
- 导入面板：
  - Tab/单选切换“本地文件 / 网络地址”。
  - 显示上传/下载进度、错误提示。
- 棋盘展示：导入成功后默认显示最终局面。
- 保存弹窗：
  - 含手数选择器、分类单选框（入门/进阶），无标题输入。
  - 显示“答案来源”单选项（SGF 主线 / KataGo 建议 / 手动录入）；不同选项会展示对应的预览或录入区域。
  - SGF 主线：显示后续几手的预览，可调整保留步数。
  - KataGo 建议：触发分析后展示 PV 列表，允许选择其一作为答案。
  - 手动录入：用户在棋盘上依序点击录入正确解，可添加多条候选路径（标记为 `primary` / `alternative`）。
  - 提交后显示保存结果与 `exerciseId`。
- AI 分析：
  - “AI 分析”按钮与手数选择器结合；展示 KataGo 返回的数据（建议点列表、胜率、PV 等）。
  - 在棋盘上叠加标记或列表展示。
- 练习页（train.html）对接：
  - 题集分类直接基于 `category`（入门 = beginner、进阶 = advanced）；在题集下拉旁展示说明，例如“当前分类：入门（基础死活） / 进阶（复杂攻防）”。
  - 题目面板中增加分类徽章，明确当前题目属于入门或进阶。
  - 新建习题后可提示“是否立即前往练习页查看”，并在练习页顶部浮层提醒“本题来源：入门/进阶”。

## 分类管理
- 当前仅支持两类：`beginner`（入门）、`advanced`（进阶）。
- 可在未来扩展更多类别，但 MVP 仅保留二选一以保证练习页面结构简洁。
- 若后续需要细分主题（如“死活”“官子”），可通过另一个扩展字段或目录结构实现，但首版暂不引入。

## 未来增强
- 保存时自动分析 SGF 分支，推荐题目起点、正确答案及标签建议（如根据 KataGo 分析判断是“死活”还是“官子”）。
- 在分析结果中增加 ownership、热力图或一键生成题目解答。
- 习题库管理：列表、搜索、标签筛选、做题记录。
- 提子规则、目数计算、Ownership 等更精细的复盘能力。
- KataGo 引擎池 / 队列管理优化，支持批量分析或长时间计算。

### `payload.json` 示例
```json
{
  "exerciseId": "ex-a1b2c3-45",
  "category": "beginner",
  "createdAt": "2024-05-18T10:20:30Z",
  "boardSize": 19,
  "komi": 7.5,
  "moveIndex": 58,
  "initialSetup": { "AB": ["pd", "dd"], "AW": ["qc"] },
  "question": {
    "toPlay": "black",
    "stones": {
      "black": ["pd", "dd", "qe"],
      "white": ["qc", "cf"]
    }
  },
  "answer": {
    "source": "sgf_mainline",
    "primary": ["qf", "pe"],
    "alternatives": [
      { "label": "KataGo PV #1", "moves": ["qf", "rf", "qd"], "winrate": 0.78 }
    ]
  },
  "analysis": {
    "katago": {
      "winrate": 0.78,
      "scoreLead": 5.3,
      "pv": ["qf", "rf", "qd"],
      "visits": 600
    }
  },
  "rawSgf": "(;GM[1]SZ[19]KM[7.5]... )"
}
```

## 后端实现补充（当前进度）
- `/api/review/import` 已支持 multipart（`sgf_file`）与 JSON（`sourceUrl`）两种入口，返回体含 `reviewId`、棋盘尺寸、`initialSetup`、`finalStones` 以及完整主线 `moves`。解析结果会写入会话级 `review_store`，TTL 默认 30 分钟。
- `/api/review/analyze` 接收 `{ reviewId, moveIndex, maxVisits? }`，自动复用缓存的 KataGo 引擎并向前端回传 `winrate/scoreLead/pv`，同一手数的分析结果会缓存在 `review_store.analysis_cache` 中。
- `/api/exercise/save` 接收 `{ reviewId, moveIndex, category, answer }`：
  - `category` 当前限定 `beginner` / `advanced`；
  - `answer.source` 支持 `sgf_mainline { length }`、`katago { pv, winrate?, scoreLead?, visits?, label?, alternatives? }`、`manual { primary, alternatives? }`；
  - 合法性校验包含手数范围、坐标格式、答案序列非空；
  - 成功时返回 `{ "exerciseId": "ex-<hash>-<moveIndex>" }`，并在 `backend/data/exercises/<exerciseId>/payload.json` 中落盘，默认附带所选 `rawSgf`。
