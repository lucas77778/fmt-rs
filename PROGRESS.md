# fmt-rs — 进展与计划

> 目标：用 Rust 重写一个 shell 命令格式化工具，替换当前基于 `mvdan-sh`(GopherJS) 的实现，
> 用于美化 Claude Code permission dialog 中展示的 Bash 命令，方便用户审阅。

最后更新：2026-06-23

---

## 一、背景与最终目标

Claude Code 在执行 Bash 工具前会弹出 permission dialog 让用户确认。模型生成的命令常常是
挤在一行、空格混乱、多条 `;` 语句堆叠的形式，可读性差。目标是在 **执行前自动格式化** 这些命令，
让 dialog 显示排版整齐的版本，降低审阅成本。

Claude Code 本体是闭源、编译进二进制的（`~/.local/share/claude/versions/<ver>`），
**无法直接修改渲染 dialog 的 Ink 组件**。因此走官方扩展点。

---

## 二、当前已落地的方案（v0，基于 mvdan-sh）

通过 **PreToolUse hook 作为 middleware** 实现：拦截 Bash 工具调用 → 格式化 `command` →
回写 `updatedInput` → dialog 显示格式化后的版本。已验证可用。

### 关键技术结论（踩坑记录）

1. **接入机制**：PreToolUse hook 返回 JSON：
   ```json
   {
     "hookSpecificOutput": {
       "hookEventName": "PreToolUse",
       "permissionDecision": "ask",
       "updatedInput": { "command": "<格式化后的命令>" }
     }
   }
   ```
2. **`permissionDecision` 必须是 `"ask"`，不能是 `"defer"`**。
   - `defer` 是 **headless 模式**（`-p --resume`）专用的暂停/重评估机制，
     在交互式会话里 **不会** 把改写后的命令拿去弹窗。（实测：用 defer 时 dialog 显示原命令）
   - `ask` + `updatedInput` 才是官方支持的 "hook as middleware" 路径
     （见 claude-code CHANGELOG：*"allow `updatedInput` when returning `ask`"*）。
3. **无改写时返回 `{}`（no-op）**：让简单命令走正常权限流程，可被 allowlist 自动放行，
   避免对每条命令都强制弹窗。代价是：被格式化改写的命令一定会弹窗（即使本可自动放行）。
4. **绝不阻断工具**：解析失败 / 非 Bash / 任意异常 → 输出 `{}` 静默放行原命令。

### 当前文件位置（v0 实现）

| 文件 | 作用 |
|---|---|
| `~/.claude/hooks/shfmt-format/format-bash.js` | hook 脚本（Node, CommonJS） |
| `~/.claude/hooks/shfmt-format/node_modules/mvdan-sh` | 格式化引擎（GopherJS 编译的 shfmt） |
| `~/.claude/settings.json` → `hooks.PreToolUse`（`matcher: "Bash"`） | 配置接线 |
| `~/.claude/settings.json.bak` | 改动前备份 |

### 格式化引擎现状

- 用的是 npm 包 `mvdan-sh`（`mvdan/sh` 的 GopherJS 产物），不是本地
  `~/Developer/shfmt`（patrickvane fork）。原因：本机无 Go 工具链，且 fork 的特色选项 `-ns`
  在 `_js/main.go` 的 JS 绑定里根本没暴露，对 Bash 格式化两者等价。
- 调用：`NewParser(KeepComments(true))` → `Parse` → `NewPrinter(Indent(2), BinaryNextLine(true), SwitchCaseIndent(true))` → `Print`。

### shfmt 实际行为（重要预期，Rust 版需对齐或有意改进）

- ✅ `;` 分隔的多语句 → 拆成多行
- ✅ 空格 / 重定向规范化（`grep   -rn TODO>out 2>&1` → `grep -rn TODO >out 2>&1`）
- ✅ `for/if/while/case/函数` 复合块 → 缩进展开
- ❌ **不会** 给单行的长 `&&` / `|` 链自动换行（shfmt 设计上从不主动 wrap 长行）

---

## 三、v0 方案的不足（驱动 Rust 重写的动机）

1. **体积大、启动慢**：`mvdan-sh`(GopherJS) 的 `index.js` ~1.7MiB，每次 hook 调用都要
   `node` 冷启动 + 加载，存在百毫秒级开销。
2. **依赖 Node 运行时**：hook 命令硬编码了 `/opt/homebrew/bin/node`，不便分发。
3. **不主动换行**：长 `&&`/`|` 链不拆行，而这正是最该被美化的场景之一。
4. **风格不可控**：受限于 shfmt 暴露的选项。
5. **传递链长**：Go → GopherJS → JS，难维护、难定制。

---

## 四、Rust 重写计划（fmt-rs，本仓库）

### 设计目标

- **单一静态二进制**，零运行时依赖，启动 < 10ms，便于分发与接入 hook。
- 行为对齐 shfmt 的「安全格式化」基线，并在其上 **可选地主动拆分长 `&&`/`|`/`;` 链**。
- 永不改变命令语义；解析失败时原样输出（保持「绝不阻断」语义）。
- I/O 契约简单：stdin 读命令 → stdout 输出格式化结果（hook 侧用一层薄 wrapper 包成
  PreToolUse JSON，或由二进制直接支持 `--hook` 模式输出 JSON）。

### 技术选型（已定）

- **解析器**：自研最小 lexer + 递归下降 parser（见下「build vs buy」）。只解析命令结构，
  表达式/复杂展开走 OPAQUE。
- **打印器**：自研 Wadler/Prettier `Doc` 引擎，宽度驱动（见下「架构」）。

### 架构（已落地）

```
命令字符串 ──lexer──▶ token 流 ──parser──▶ ast::File ──printer──▶ doc::Doc ──pretty()──▶ 格式化输出
            (在写)              (待写)        ✅ src/ast.rs   ✅ printer.rs   ✅ doc.rs
```

- **`src/ast.rs`**：忠实移植 mvdan/sh 的 `nodes.go`+`tokens.go`（15 种 Command、8 种 WordPart、11 组操作符）。带 `Pos{offset,line,col}`。
- **`src/doc.rs`**：Wadler/Prettier `Doc` 代数 + Lindig "Strictly Pretty" 宽度驱动布局引擎（`text/line/soft_line/hard_line/concat/nest/group/if_break`）。`group` = 放得下单行、放不下折行，这是 M2 的原生能力。
- **`src/printer.rs`**：`ast → Doc`，**完全不读 `Pos`**（纯宽度驱动）。
- 三个模块共 ~1100 行，测试全绿。

### 设计裁决（2026-06-23，四视角分析 + 对抗审查，详见 artifact「fmt-rs 最终范围规格」）

应用场景只限 Claude Code permission dialog，据此大幅收窄范围：

**OPAQUE（不解析，原样逐字输出）—— 最大的一刀：**
- `$(())`/`(())` 算术、`[[ ]]`/`[ ]` test、`${...}` 复杂操作符、数组 RHS `=(...)`、反引号、extglob、heredoc body。
- 取原文方式优先 **字节偏移切片 `&src[start..end]`**（零误差）；次选 **quote-aware + `$()` 嵌套计深扫描器**（~30 行，共享）。
- **禁止 naive 括号计数**：`${VAR:-$(awk 'END{print NR}')}` / `'foo)bar'` 会让朴素扫描器静默截断 = 语义危险。

**DROP（遇到即 bail，返回原命令）：** `FuncDecl`、`select`、`coproc`、`time`、mksh 变体（`${ ;}`/`${| ;}`/`|&` coprocess）、废弃 `$[ ]`。

**⚠️ 对抗审查拦下、不得省的：**
- **注释 `#`** → 检测到任何 Comment 节点立即 **bail**（注释是用户审批依据，删了等于骗审批）。
- **heredoc `<<`/`<<-`** → 即使 body 不格式化也**必须检测 + flush**，否则后续 token 被当 body 静默误解析。
- **进程替换 `<()`/`>()`、后台 `&`、`export FOO=... && cmd`** → 保留（真实 agent 命令）。

**🚫 永不实现清单（防止日后误加）：**
`BinaryNextLine`、`KeepPadding`/`tabwriter` 列对齐、`Minify`、`FunctionNextLine`、`SwitchCaseIndent`、`SpaceRedirects` 选项、`simplify` pass、空行保留、`ParamExp` 短形式归一化、源位置驱动的 `\`-换行。

### dialog 硬约束

- **`DEFAULT_WIDTH = 80`**（边框/padding 占 8，可用 ~72；支持 `FMTRS_WIDTH` 覆盖）。
- **真实换行完整保留**（Ink `Text` 不重写换行）。
- **输入 > 8000 字节 → 直接 passthrough**（hook stdout 上限 10k）。
- **不得输出 ANSI / tab**。
- **M2 = 做**：flat 宽度 **> 60 字符**触发折行，operator-at-end，缩进 2，由现有 `Doc::group` 表达，零额外成本。

### build vs buy：自研

调研了 `brush-parser`/`tree-sitter-bash`/`yash-syntax`/`conch-parser`。结论自研：范围已缩到 650–850 行，且我们需要的「字节偏移 + 检测注释 + 检测 heredoc」恰是 brush-parser 的三个缺口（位置 13+ TODO、丢注释、heredoc location 返 None）。tree-sitter-bash 留作 M4 测试 oracle，不做运行时依赖。

### 里程碑（修订后）

- [x] **M-1（完成）**：AST IR + Doc 引擎 + printer（`ast.rs`/`doc.rs`/`printer.rs`）
- [x] **M0（完成）**：自研 lexer（`lexer.rs`）—— 词/操作符/引号/转义/UTF-8、注释检测、**heredoc 检测 + flush**、共享 quote-aware opaque 扫描器（`scan_word`/`paren_group`/`dollar_brace`）、字节偏移；含 corpus round-trip property 测试
- [x] **M1（完成）**：自研递归下降 parser（`parser.rs`）—— and-or/管道/简单命令/重定向(含 fd)/Subshell/Block/if-elif-else/while-until/for；`[[ ]]`/`(( ))` 字节偏移 opaque 切片；驱动层 `format.rs`（>8k passthrough、注释 bail、heredoc/funcDecl/case bail、**输出 word 多集校验**）；`main.rs` stdin→stdout，`FMTRS_WIDTH` 可覆盖；端到端跑通 seed 用例
- [x] **M2（完成）**：长链折行由 `Doc::group` + break propagation 实现，operator-at-end + 2 空格缩进；实测 `cd && npm ci && …`、管道链按宽度逐行展开
- [x] **M3（完成，已上线）**：`--hook` 模式 —— 读 PreToolUse JSON、输出 `ask`+`updatedInput`（改写时）或 `{}`（无改写/非 Bash/任何错误）。零依赖手写 JSON 解析/编码（`json.rs`）。已替换 `~/.claude/settings.json` 的 Bash hook，**fmt-rs 现已接管 dialog 格式化**（5.4ms，旧 node hook ~100ms+）
- [ ] **M4**：扩充压测到 1000 条（bail 率 <5%，每条输出过 `bash -n`）；边界测试（引号内 `]]`、heredoc 终止符前缀匹配、同行多 heredoc）；把压测器 + 语料落进仓库 `tests/`；观察真实使用、收集 bail 样本迭代

**进度**：M-1~M3 完成,共 8 个源文件 ~2300 行 + 72 单测全过 + 205 条对抗压测 0 失败；release 二进制 590K,hook 5.4ms。剩 M4(扩展压测 + 长期观察)。

**实测样例**（`FMTRS_WIDTH=40`）：
```
cd /repo && npm ci && npm run build && npm test && npm run deploy
```
→
```
cd /repo &&
  npm ci &&
  npm run build &&
  npm test &&
  npm run deploy
```

### 替换 v0 的切换点（已执行，2026-06-23）

- **二进制安装位置**：`~/.claude/hooks/fmt-rs/fmt-rs`（从 `target/release` 拷贝，独立于 dev target 目录，`cargo clean` 不影响）。
- **`settings.json` 改动**：`PreToolUse` / `matcher:"Bash"` 的 command 由
  `/opt/homebrew/bin/node .../shfmt-format/format-bash.js`
  改为 `/Users/ryo/.claude/hooks/fmt-rs/fmt-rs --hook`（`timeout:10` 等其余不变）。
- **备份**：`~/.claude/settings.json.pre-fmtrs.<UTC时间戳>`。旧 v0 脚本 `~/.claude/hooks/shfmt-format/` 保留未删，回滚只需把 command 改回去。
- **重装新版二进制**：`cargo build --release && cp target/release/fmt-rs ~/.claude/hooks/fmt-rs/fmt-rs`。

### 测试用例种子（从 v0 实测取）

输入：
```
x=1;   y=2 ;  for i in 1 2 3; do echo "iteration $i" ; done ;   if [ "$x" -lt "$y" ]; then echo   "x is smaller" ; fi ;  ls -la /tmp  >/dev/null   2>&1 ;   echo    "all done"
```
期望输出（shfmt 基线）：
```
x=1
y=2
for i in 1 2 3; do echo "iteration $i"; done
if [ "$x" -lt "$y" ]; then echo "x is smaller"; fi
ls -la /tmp >/dev/null 2>&1
echo "all done"
```

边界用例（必须原样放行，不得 panic / 不得改写）：
- 非 Bash 工具
- 空命令 / 纯空白
- 不可解析：`$((foo); (bar))`
- 已规范的命令（输出应等于输入 → no-op）
