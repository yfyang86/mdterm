# mdterm

一款面向 AI 编码 CLI（Claude Code、Kimi CLI、Codex，或任何输出 mdterm
自定义 JSONL 格式的工具）的**字节级透传 PTY 代理**，并附带 Markdown
渲染面板。被代理的 CLI 行为与直接运行完全一致；其会话记录（transcript）
由 mdterm 实时渲染到自有面板（浏览器面板、终端分页器、tmux 弹窗），
而非注入到 CLI 自身的屏幕中。

## 工作区结构

```
mdterm/
├── crates/
│   ├── mdterm-core/     # 会话记录模型、发现、监听、解析
│   ├── mdterm-pty/      # PTY 代理 + 热键拦截
│   ├── mdterm-viewer/   # HTML 查看器服务（axum + SSE）
│   ├── mdterm-render/   # 终端 ANSI 渲染器（Sprint 2）
│   └── mdterm-cli/      # `mdterm` 可执行文件、子命令装配
```

## 构建

需要稳定版 Rust 工具链（开发基于 1.98；edition 2021）。

```bash
cargo build --release          # 产物位于 target/release/mdterm
cargo test --workspace         # 运行测试套件
```

## 安装

```bash
cargo install --path crates/mdterm-cli
# 或将可执行文件复制到 PATH 上的任意位置：
cp target/release/mdterm ~/.local/bin/
```

## 使用方法

### Claude Code

```bash
mdterm wrap                       # 自动检测 PATH 上的 `claude`
mdterm wrap -- claude --debug     # 显式指定命令及其参数
```

在代理模式下，按下热键组合（见下文）：`Ctrl-G b` 打开当前会话的实时
浏览器查看器；`Ctrl-G r` 在终端面板（tmux 弹窗 / `less -R` / 直接
输出，详见下文"终端渲染面板"）上渲染最后一条助手消息。

### Kimi CLI

```bash
mdterm wrap -- kimi
```

### Codex

```bash
mdterm wrap -- codex
```

`mdterm wrap` 在不指定命令时，会依次按 `claude` → `kimi` → `codex`
的顺序在 PATH 上进行自动检测。

### 提供商与会话记录

所有读取会话记录的命令均接受 `--provider <claude|codex|kimi|
selfdefined>` 与 `--transcript <path>` 参数：

- **两者均不指定**时，使用**跨提供商最新的会话记录**（按修改时间排序；
  详见下文各根目录），并使用对应提供商的解析器。
- **仅指定 `--transcript`** 时，文件格式从**内容嗅探**得到——对于
  显式指定的文件，提供商标志可选。
- **仅指定 `--provider`** 时，发现范围限定为该提供商的根目录。
  `--provider selfdefined` 无根目录，必须配合 `--transcript` 使用。

发现根目录（取最新的 `*.jsonl`）：

| 提供商 | 根目录 |
|--------|--------|
| claude | `~/.claude/projects/**/` |
| kimi   | `~/.kimi-code/sessions/**/`、`~/.kimi/sessions/**/`（旧版探测：`~/.kimi/projects/**/`、`~/.config/kimi/**/sessions`） |
| codex  | `~/.codex/sessions/**/` |

解析格式：Claude Code 会话 JSONL；kimi-code 线路协议（`agent.message.appended`，
协议版本 ≥ 1.5），旧版 kimi 线路协议（`TurnBegin`/`ContentPart`）及
kimi `context.jsonl`；Codex rollouts（`response_item` 消息与工具调用，
回退到 `event_msg`）；自定义格式（见下文）。Kimi 的 `think` 块将被跳过；
工具调用以围栏 JSON 形式汇总；工具输出以围栏 `Tool` 消息渲染。

### 自定义会话记录格式

任何工具（或脚本）均可通过写入如下规范的 JSONL 来接入 mdterm——
每行一个 JSON 对象：

```json
{"type":"metadata","sessionId":"my-session","title":"optional"}
{"type":"message","role":"user","content":"markdown text","timestamp":"2026-09-30T13:00:00Z"}
{"type":"message","role":"assistant","content":[{"type":"text","text":"..."}],"model":"..."}
```

- `type`：`metadata`（可选，通过 `sessionId` / `session_id` / `id` 设置
  会话标识）或 `message`。
- `role`：`user` | `assistant` | `system` | `tool`（其他取值将被跳过）。
- `content`：Markdown 字符串，或由 `{"type":"text","text":…}` 块构成的
  数组（以空行连接）。
- `timestamp`、`model`：可选字符串，原样传递至界面。
- 未知或格式错误的行将被跳过，不会导致致命错误。

### 独立查看器

```bash
mdterm serve [--transcript <path>] [--port N]   # 默认端口 0 = 由操作系统分配
```

监听会话记录，并将查看器服务于 `http://127.0.0.1:<port>/`。路由：
`/`（单页应用）、`/events`（全量会话 JSON 的 SSE 流）、`/api/session`
（当前会话快照）。

### 一次性渲染

```bash
mdterm render [--last|--all] [--transcript <path>]
              [--file <path.md>] [--math auto|unicode|image|off] [--width N]
```

将 ANSI 直接输出到 stdout。默认渲染整段会话（`--all`）；`--last` 仅
选取最后一条助手消息。`--file` 渲染纯 Markdown 文件而非会话记录。
`--width` 默认为终端宽度（非 TTY 时为 100）。`--math` 控制
`$...$` / `$$...$$` 数学公式的渲染（详见下文"数学公式渲染"，默认 `auto`）。

### 终端能力探测

```bash
mdterm caps
```

输出探测到的终端能力：终端程序、图形协议（`kitty` / `iterm2` / `sixel` /
`none`）、真彩支持，以及原始的 `TERM` / `TERM_PROGRAM` / `COLORTERM` /
`TMUX` 取值。

基于环境变量进行检测（首个匹配项生效）：

| 信号 | 结果 |
|------|------|
| `TERM_PROGRAM=iTerm.app` | iTerm2 内联图像（OSC 1337） |
| `KITTY_WINDOW_ID` 已设置，或 `TERM` 包含 `kitty` | Kitty 图形协议 |
| `TERM_PROGRAM=WezTerm` / `WEZTERM_*`、`TERM_PROGRAM=ghostty` / `GHOSTTY_*` | Kitty 图形协议（wezterm 与 ghostty 均实现了该协议） |
| `TERM` 包含 `foot` 或 `sixel` | sixel |
| `COLORTERM=truecolor`（或 `24bit`） | 真彩（truecolor） |

## 终端渲染面板（`Ctrl-G r`）

按下 `Ctrl-G r` 时，`wrap` 子命令使用 `mdterm-render` 渲染被监听
会话记录中的最后一条助手消息，并展示在当前可用的最佳面板上：

1. **tmux 弹窗** —— 当 `$TMUX` 已设置且 `tmux` 在 PATH 上时：
   `tmux display-popup -E -w 90% -h 85% "less -R <file>"`（若 `less`
   缺失，则使用基于 `cat` 的提示界面）。弹窗的终端由 tmux 管理，
   因此代理在此处无需特殊处理。
2. **`less -R`** —— 否则，当 `less` 在 PATH 上时。渲染后的 ANSI 将
   写入临时文件；`less` 作为代理的子进程运行于真实终端之上。代理
   首先**挂起**原始模式（恢复 cooked termios 并暂停其 stdin 泵，
   使 `less` 独占终端输入），随后在 `less` 退出时**恢复**原始模式。
   该机制通过 `mdterm-pty` 中的 `PtyProxy::spawn_with_terminal_guard`
   与 `TerminalGuard::{suspend,resume}` 实现，从而确保分页器不会
   破坏代理的终端状态。
3. **直接输出** —— 当 tmux 与 less 均不可用时的最后兜底方案：渲染
   后的文本在标记之间输出至 stdout（期间恢复 cooked 模式）；子进程
   TUI 将在下一帧重绘时覆盖该内容。

## 数学公式渲染

`$...$`（行内）与 `$$...$$`（独立公式）段将在 Markdown 解析之前被拦截
（代码块/行内代码中的同名段保持字面量），并按 `--math` / `MathMode`
进行渲染：

- `auto`（默认）：当终端具备图形协议时使用图像渲染，否则使用
  Unicode 近似。
- `unicode`：始终使用 Unicode 近似——无需任何外部工具。
- `image`：强制使用图像链路；失败时回退至 Unicode。
- `off`：将 TeX 源码作为字面文本保留。

**图像回退链路**（每一步失败都将降级一档；永不发生 panic）：

1. `typst` CLI —— 单行数学文档（常见的 LaTeX 构造被转换为 typst 数学
   语法），编译为 PNG。
2. `latex` + `dvipng` —— `standalone` 文档 → DVI → 紧密裁剪的 PNG。
3. Unicode 近似。

渲染得到的 PNG 按检测到的图形协议显示：Kitty APC 图形转义（base64，
`a=T,f=100`，4096 字节分块）、iTerm2 OSC 1337，或 sixel——sixel 仅在
PATH 上存在 `img2sixel` 转换器时可用，否则链路降级至 Unicode。

**Unicode 近似覆盖范围**：希腊字母、常用运算/关系符号（`\sum`→∑、
`\int`→∫、`\le`→≤、`\times`→×，以及箭头、集合运算符等），
`\frac{a}{b}`→`a/b`（复合分式加括号），`\sqrt{x}`→`√(x)`，
上标/下标映射至真正的 Unicode 上下标字形（不存在时回退为 `^(...)` /
`_(...)`），尺寸/间距命令被丢弃。

## 热键

仅在 `mdterm wrap` 代理模式下生效（可通过 `--hotkeys off` 关闭）。
组合键仅在完全匹配时生效；其他所有输入均原样透传至 CLI。

| 组合键      | 动作                                                |
|-------------|-----------------------------------------------------|
| `Ctrl-G r`  | 在终端面板（tmux 弹窗 / 分页器）上渲染最后一条助手消息 |
| `Ctrl-G b`  | 打开/聚焦当前会话的实时浏览器查看器                  |

## 浏览器查看器

低饱和度深色单页应用。渲染 GFM Markdown（表格、带语法高亮的围栏代码）、
KaTeX 数学公式（`$...$`、`$$...$$`、`\(...\)`、`\[...\]`）以及 mermaid
图表。自动滚动至最新消息；向上滚动可暂停跟随，向下滚回底部可恢复跟随。
通过 SSE 在每次会话记录变更时实时重渲染；顶部显示连接状态
（`live` / `reconnecting`）。

所有前端资源均**随项目分发**（编译时嵌入至二进制；运行时无 CDN 请求）：

| 资源 | 版本 | 许可证 |
|------|------|--------|
| markdown-it | 14.1.0 | MIT |
| highlight.js（含 github-dark 主题） | 11.10.0 | BSD-3-Clause |
| KaTeX（含 woff2 字体） | 0.16.11 | MIT |
| mermaid | 11.4.1 | MIT |

## 故障排除

- **`no transcript found ... pass --transcript <path>`** —— mdterm 在
  提供商根目录下查找最新的 `*.jsonl`（见上文"提供商与会话记录"）。
  若尚无 CLI 写入会话（或 `$HOME` 与预期不同），请显式传入文件路径，
  格式将被自动嗅探。
- **识别到错误的提供商** —— 使用 `--provider <claude|codex|kimi|
  selfdefined>` 锁定解析器。嗅探依据特定的行形态进行，因此若文件
  前 200 行不包含任何可识别行，将回退至 Claude 解析器（其可容错地
  跳过未知行）。
- **浏览器无法打开** —— `wrap`/`render` 在 macOS 下使用 `open`、
  在 Linux 下使用 `xdg-open`。若两者均失效（无 GUI / SSH 场景），
  请将打印输出的 `http://127.0.0.1:<port>/` URL 复制至任意浏览器；
  远程会话请使用 SSH 端口转发：`ssh -L <port>:127.0.0.1:<port> host`。
- **查看器显示旧会话** —— 发现机制选取的是*最新*会话记录；其他项目
  的较旧会话可能会胜出。请使用 `--transcript` 显式指定。
- **热键无响应** —— 检查是否已被禁用（`--hotkeys off`），并确认组合键
  以序列方式按下（Ctrl-G，松开，再按 `r` / `b`）。
- **调试日志** —— 设置 `RUST_LOG=mdterm=debug`（日志输出至 stderr）。

## 开发

```bash
cargo build --workspace
cargo test --workspace
```

迭代进度：**Sprint 1**（core/pty/viewer/cli 与浏览器面板）与
**Sprint 2**（`mdterm-render` 终端 ANSI 渲染器，含数学公式/图像、
能力探测、tmux/分页器终端面板、`render --file/--math/--width`、
`caps` 子命令）均已完成。Sprint 之后：多提供商会话记录支持
（kimi/codex/selfdefined 解析器、格式嗅探、跨提供商发现）以及
F7 死键修复。

## 作者

Yifan Yang <yfyang.86@hotmail.com>

## 许可证

MIT —— 详见 [LICENSE](LICENSE)。

随项目分发的浏览器查看器前端资源（见上文"浏览器查看器"）保留各自
原始许可证，均为宽松且与 MIT 兼容：markdown-it（MIT）、
highlight.js（BSD-3-Clause）、KaTeX（MIT）、mermaid（MIT）。