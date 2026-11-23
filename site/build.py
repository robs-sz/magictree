#!/usr/bin/env python3
"""Build the GitHub Pages site into a directory (default: site/_dist).

The landing page is static: it is copied as it is. The four reference pages
are the repository's own docs/*.md, rendered through site/doc.html, so the
site and the Markdown GitHub shows can never say different things.

    pip install markdown
    python3 site/build.py [out-dir]
"""

import re
import shutil
import sys
from pathlib import Path

import markdown

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
REPO = "https://github.com/robs-sz/magictree"

# The order of the navigation, and what each page is about for a search result.
DOCS = [
    ("worktrees", "Onboarding a repository, creating and removing worktrees, port modes and browser aliases."),
    ("commands", "Every magictree command, the TUI, shell completion, shell activation and updating."),
    ("manifest", "Everything magictree.toml can say: services, ports, health, bootstrap, jobs and monorepos."),
    ("configuration", "Machine-wide settings, and where magictree keeps its state."),
]


def render(name: str) -> tuple[str, str, list]:
    """One doc as (title, html, its second-level headings)."""
    text = (ROOT / "docs" / f"{name}.md").read_text()
    # Links that leave docs/ point at the repository; links between docs stay
    # on the site.
    text = text.replace("](../", f"]({REPO}/blob/main/")
    text = re.sub(r"\]\((\w+)\.md(#[\w-]+)?\)", r"](\1.html\2)", text)
    md = markdown.Markdown(extensions=["tables", "fenced_code", "toc", "sane_lists"])
    html = md.convert(text)
    # A wide table scrolls inside its own box instead of widening the page.
    html = html.replace("<table>", '<div class="table"><table>').replace("</table>", "</table></div>")
    title = md.toc_tokens[0]["name"] if md.toc_tokens else name.title()
    sections = md.toc_tokens[0]["children"] if md.toc_tokens else []
    return title, html, sections


def navigation(current: str, titles: dict, sections: list) -> str:
    lines = []
    for name, _ in DOCS:
        here = name == current
        attr = ' aria-current="page"' if here else ""
        line = f'          <li><a href="{name}.html"{attr}>{titles[name]}</a>'
        if here and sections:
            items = "".join(f'<li><a href="#{s["id"]}">{s["name"]}</a></li>' for s in sections)
            line += f"<ul>{items}</ul>"
        lines.append(line + "</li>")
    return "\n".join(lines)


def main() -> None:
    out = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else SITE / "_dist"
    if out.exists():
        shutil.rmtree(out)
    (out / "assets").mkdir(parents=True)
    (out / "docs").mkdir()

    for name in ("index.html", "styles.css", "app.js"):
        shutil.copy(SITE / name, out / name)
    for asset in (SITE / "assets").iterdir():
        shutil.copy(asset, out / "assets" / asset.name)
    # The README's pictures are the site's pictures.
    for name in ("banner.svg", "banner-dark.svg", "tui.svg"):
        shutil.copy(ROOT / "docs" / name, out / "assets" / name)
    shutil.copy(ROOT / "install.sh", out / "install.sh")
    # No Jekyll: the files are served as they are.
    (out / ".nojekyll").write_text("")

    template = (SITE / "doc.html").read_text()
    pages = {name: render(name) for name, _ in DOCS}
    titles = {name: page[0] for name, page in pages.items()}
    for name, description in DOCS:
        title, html, sections = pages[name]
        page = (
            template.replace("{{title}}", title)
            .replace("{{description}}", description)
            .replace("{{nav}}", navigation(name, titles, sections))
            .replace("{{content}}", html)
            .replace("{{source}}", f"{REPO}/blob/main/docs/{name}.md")
        )
        (out / "docs" / f"{name}.html").write_text(page)

    print(f"built {out.relative_to(ROOT) if out.is_relative_to(ROOT) else out}")


if __name__ == "__main__":
    main()
