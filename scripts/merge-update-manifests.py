#!/usr/bin/env python3
"""Merge per-platform updater JSON files into latest.json.

Reads every latest-*.json in the target directory and merges their platform
entries into a single latest.json (Windows-only project: the Windows manifests
are the only inputs).
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def load_manifests(directory: Path) -> list[tuple[Path, dict]]:
    items: list[tuple[Path, dict]] = []
    for path in sorted(directory.glob("latest-*.json")):
        if path.name == "latest.json":
            continue
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(data, dict):
            continue
        items.append((path, data))
    return items


def merge_manifests(items: list[tuple[Path, dict]]) -> dict | None:
    if not items:
        return None
    merged: dict = {
        "version": "",
        "notes": "",
        "pub_date": "",
        "platforms": {},
    }
    for _, data in items:
        version = str(data.get("version") or "").strip()
        if version:
            merged["version"] = version
        notes = data.get("notes")
        if isinstance(notes, str) and notes:
            merged["notes"] = notes
        pub_date = str(data.get("pub_date") or "")
        if pub_date > str(merged.get("pub_date") or ""):
            merged["pub_date"] = pub_date
        platforms = data.get("platforms")
        if isinstance(platforms, dict):
            merged["platforms"].update(platforms)
    if not merged["version"] or not merged["platforms"]:
        return None
    return merged


def merge_directory(directory: Path) -> Path | None:
    items = load_manifests(directory)
    merged = merge_manifests(items)
    if merged is None:
        return None
    latest = directory / "latest.json"
    latest.write_text(json.dumps(merged, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    return latest


def _self_test() -> None:
    import tempfile

    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        # 三个平台各自产出的清单，合并后必须**一个都不少**。
        #
        # ⚠ 这个自检此前只覆盖 Windows —— 而"只有 Windows"正是当时
        # 线上真实的缺陷（mac / linux 客户端「检查更新」报错，因为
        # latest.json 里没有它们平台的键）。自检覆盖面与真实覆盖面
        # 不一致时，它就证明不了最该证明的那件事。
        fixtures = {
            "latest-windows-x86_64.json": {
                "windows-x86_64-nsis": "https://example/win-setup.exe",
                "windows-x86_64": "https://example/win-setup.exe",
            },
            "latest-darwin-aarch64.json": {
                "darwin-aarch64-app": "https://example/mac-arm64.app.tar.gz",
                "darwin-aarch64": "https://example/mac-arm64.app.tar.gz",
            },
            "latest-darwin-x86_64.json": {
                "darwin-x86_64-app": "https://example/mac-x64.app.tar.gz",
                "darwin-x86_64": "https://example/mac-x64.app.tar.gz",
            },
            "latest-linux-x86_64.json": {
                "linux-x86_64-appimage": "https://example/linux.AppImage.tar.gz",
                "linux-x86_64": "https://example/linux.AppImage.tar.gz",
            },
        }
        expected: set[str] = set()
        for name, platforms in fixtures.items():
            (root / name).write_text(
                json.dumps(
                    {
                        "version": "0.1.19",
                        "notes": "",
                        "pub_date": "2026-08-21T00:00:02Z",
                        "platforms": {
                            k: {"signature": "sig", "url": v}
                            for k, v in platforms.items()
                        },
                    }
                ),
                encoding="utf-8",
            )
            expected.update(platforms)

        latest = merge_directory(root)
        assert latest is not None
        merged = json.loads(latest.read_text(encoding="utf-8"))
        assert merged["version"] == "0.1.19"
        assert set(merged["platforms"]) == expected, (
            f"合并后缺平台：{expected - set(merged['platforms'])}"
        )
        # 每个平台都要带签名与 URL —— 空壳条目等于没有。
        for key, entry in merged["platforms"].items():
            assert entry.get("signature"), f"{key} 缺 signature"
            assert entry.get("url"), f"{key} 缺 url"
        print(f"merge-update-manifests: self-test ok（{len(expected)} 个平台键）")


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        _self_test()
        return 0
    directory = Path(sys.argv[1] if len(sys.argv) > 1 else ".")
    latest = merge_directory(directory)
    if latest is None:
        print("merge-update-manifests: 未找到可合并的 latest-*.json", file=sys.stderr)
        return 1
    print(f"merge-update-manifests: 已生成 {latest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
