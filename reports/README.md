# reports/

存放**本机生成**的磁盘扫描结果 —— 由 `diskdoctor` 的扫描命令或
`survey` 子命令输出，例如：

| 文件 | 内容 |
|---|---|
| `report-C.md` | Markdown 体检报告（人读） |
| `result-C.json` | 完整分析结果（程序读） |
| `我的磁盘内容.html` | 可浏览的决策界面（自包含，双击即开） |

## 为什么不进版本控制

这些文件记录的是**运行它的这台机器**的目录结构和文件名。
提交上来等于把使用者的磁盘情况公开，所以 `.gitignore` 排除了
`reports/` 下的全部内容，只留这份说明。

克隆下来时这个目录里只有本文件；跑过一次扫描后才会出现你自己的报告。

## 会扫描出什么

```bash
diskdoctor "C:\" --out reports/report-C.md --json reports/result-C.json
diskdoctor survey "C:\Users\<你的用户名>" --html reports/我的磁盘内容.html
```

> 建议以管理员身份跑：否则 `$Recycle.Bin`、部分 `AppData`、卷影副本
> 等位置读不到，报告会标注「因权限跳过 N 处」。
