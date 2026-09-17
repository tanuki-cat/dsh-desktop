#!/usr/bin/env python3
"""汇总随包分发组件的许可信息，生成 THIRD-PARTY-NOTICES.md（方案 §10）。

用法: write_third_party_notices.py <runtime 目录> <node 版本> <dsh 版本> <pnpm 版本> <dshmarket 版本>

覆盖：Node / npm / pnpm / @deepseek-ai/dsh 及其依赖树 / 随包附带的插件市场 dshmarket。
只读取各包 package.json 的 name/version/license，不做推断 —— 缺失就写 UNKNOWN，便于人工复核。
"""
import json
import os
import sys


def packages_under(root):
    """收集 root 下 node_modules 里的一层包（含 @scope/name），不递归进包内部。"""
    found = {}
    for dirpath, dirnames, filenames in os.walk(root):
        if os.path.basename(dirpath) != "node_modules":
            continue
        entries = list(dirnames)
        for entry in entries:
            if entry.startswith("@"):
                scope_dir = os.path.join(dirpath, entry)
                try:
                    scoped = sorted(os.listdir(scope_dir))
                except OSError:
                    continue
                candidates = [os.path.join(scope_dir, name) for name in scoped]
            else:
                candidates = [os.path.join(dirpath, entry)]
            for candidate in candidates:
                manifest = os.path.join(candidate, "package.json")
                if not os.path.isfile(manifest):
                    continue
                try:
                    with open(manifest, encoding="utf-8") as handle:
                        data = json.load(handle)
                except (OSError, ValueError):
                    continue
                name = data.get("name") or os.path.basename(candidate)
                version = data.get("version") or "?"
                license_name = data.get("license")
                if isinstance(license_name, dict):
                    license_name = license_name.get("type")
                if not isinstance(license_name, str) or not license_name.strip():
                    license_name = "UNKNOWN"
                found[f"{name}@{version}"] = license_name.strip()
    return found


def main():
    # A Windows console defaults to a legacy code page (cp1252); these messages are UTF-8.
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass

    if len(sys.argv) != 6:
        print(__doc__, file=sys.stderr)
        return 2
    root, node_version, dsh_version, pnpm_version, market_version = sys.argv[1:]

    lines = [
        "# 第三方组件许可（随包分发）",
        "",
        f"由 `scripts/write_third_party_notices.py` 生成（registry 固定版本，见 Makefile）。",
        "",
        "## 运行时与工具",
        "",
        "| 组件 | 版本 | 许可 |",
        "|---|---|---|",
        f"| Node.js | {node_version} | MIT（发行版自带 LICENSE） |",
        f"| npm | 随 Node 发行版 | Artistic-2.0 |",
        f"| pnpm | {pnpm_version} | MIT |",
        f"| @deepseek-ai/dsh | {dsh_version} | 见其 package.json |",
        f"| dshmarket（插件市场） | {market_version} | MIT（github.com/dsh-market/dsh-market） |",
        "",
    ]

    sections = [
        ("@deepseek-ai/dsh 依赖树", os.path.join(root, "dsh-prefix")),
        ("profile 模板（插件市场及其依赖）", os.path.join(root, "profile-template")),
    ]
    for title, path in sections:
        packages = packages_under(path)
        lines += [f"## {title}", "", f"共 {len(packages)} 个包。", ""]
        lines += ["| 包 | 许可 |", "|---|---|"]
        for name in sorted(packages):
            lines.append(f"| {name} | {packages[name]} |")
        lines.append("")

    lines += [
        "## 说明",
        "",
        "- 只随包分发插件市场 dshmarket；其余第三方插件由用户自行安装，许可与责任归其作者。",
        "- `UNKNOWN` 表示该包的 package.json 未声明 license 字段，发布前需要人工确认。",
        "",
    ]

    out = os.path.join(root, "THIRD-PARTY-NOTICES.md")
    with open(out, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines))
    unknown = sum(1 for line in lines if "| UNKNOWN |" in line)
    print(f"已写入 {out}（UNKNOWN 条目 {unknown} 个）")
    # An UNKNOWN license is a distribution question, not a formatting one: shipping a tree whose
    # license nobody could name needs a deliberate decision, so failing here makes that decision
    # visible instead of leaving it in a table nobody reads (review D4).
    if unknown:
        print(
            f"{unknown} 个包没有声明 license：确认许可后再发布（不要只改这一行）",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
