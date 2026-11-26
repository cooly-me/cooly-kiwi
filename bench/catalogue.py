#!/usr/bin/env python3
"""Generate a catalogue corpus: many small metadata atoms, one document each.

A catalogue holds one short record per item (a service offered, or a proposal for a new
one): title, group, status, tags, a one-line summary, the creator, dates and a link.
Agents search it for duplicates and similar ideas before proposing something new. This
script writes such a corpus with planted near-duplicates, as a Git repository, with
questions and principals for `kiwi bench`:

    python3 bench/catalogue.py /tmp/catalogue --items 10000
    kiwi bench /tmp/catalogue --questions /tmp/catalogue/bench/questions.yaml \
        --principals /tmp/catalogue/bench/principals.yaml --embedder hash

Each question searches for one planted cluster (an item and its one or two rewordings,
written by other creators) with a third wording, a third of them typed without
diacritics; its answers are every item of the cluster. Output is deterministic for a
given `--seed`. Only the Python standard library is used.
"""

import argparse
import datetime
import os
import random
import subprocess
import unicodedata

GROUPS = {
    "food": [
        ("nấu", ["nấu", "đứng bếp", "làm"], ["cơm nhà", "món Huế", "bún chả", "phở bò", "lẩu", "đồ chay"]),
        ("dạy nấu", ["dạy nấu", "hướng dẫn nấu", "chỉ cách nấu"], ["món miền Tây", "bánh xèo", "gỏi cuốn", "cà ri", "bánh mì"]),
        ("dẫn đi ăn", ["dẫn đi ăn", "đưa đi ăn", "tour ăn"], ["đồ ăn vặt", "quán vỉa hè", "hải sản", "ốc", "cà phê trứng"]),
    ],
    "travel": [
        ("dẫn tham quan", ["dẫn tham quan", "hướng dẫn tham quan", "đưa đi chơi"], ["phố cổ", "chợ nổi", "làng gốm", "đồi chè", "bảo tàng"]),
        ("chở xe máy", ["chở xe máy", "đèo xe máy", "lái xe máy chở"], ["đi đèo", "vòng quanh hồ", "ra biển", "lên núi", "qua cầu"]),
        ("đi cùng", ["đi cùng", "đi chung", "đồng hành"], ["leo núi", "chạy bộ", "đi chợ", "đi bộ đêm", "cắm trại"]),
    ],
    "learning": [
        ("dạy", ["dạy", "kèm", "hướng dẫn"], ["tiếng Anh giao tiếp", "đàn guitar", "vẽ màu nước", "cờ tướng", "bơi"]),
        ("luyện", ["luyện", "tập", "rèn"], ["phát âm", "thuyết trình", "phỏng vấn xin việc", "viết tay", "hát karaoke"]),
        ("trò chuyện", ["trò chuyện", "nói chuyện", "tâm sự"], ["về nghề", "về sách", "về du học", "về khởi nghiệp", "về nuôi con"]),
    ],
    "home": [
        ("sửa", ["sửa", "chữa", "khắc phục"], ["xe đạp", "quạt điện", "ống nước", "máy tính", "đồ gỗ"]),
        ("dọn", ["dọn", "dọn dẹp", "sắp xếp"], ["tủ quần áo", "phòng trọ", "ban công", "nhà kho", "bàn làm việc"]),
        ("trông", ["trông", "chăm", "để mắt"], ["thú cưng", "cây cảnh", "nhà khi đi vắng", "bé đi học về", "ông bà"]),
    ],
    "events": [
        ("chụp ảnh", ["chụp ảnh", "chụp hình", "làm nhiếp ảnh"], ["kỷ yếu", "đám cưới nhỏ", "sinh nhật", "chân dung", "sản phẩm"]),
        ("làm MC", ["làm MC", "dẫn chương trình", "cầm micro"], ["tiệc công ty", "đám cưới", "sinh nhật bé", "họp lớp", "ra mắt sách"]),
        ("hát", ["hát", "biểu diễn", "đàn hát"], ["acoustic", "bolero", "nhạc Trịnh", "nhạc thiếu nhi", "cải lương"]),
    ],
    "company": [
        ("xếp hàng", ["xếp hàng", "giữ chỗ", "đứng chờ"], ["mua vé concert", "làm hộ chiếu", "khám bệnh", "mua bánh trung thu", "mua iPhone"]),
        ("làm bạn", ["làm bạn", "đi cùng", "ngồi cùng"], ["xem phim", "đi cà phê", "đi bảo tàng", "tập gym", "đi hội sách"]),
        ("nghe", ["nghe", "lắng nghe", "ngồi nghe"], ["kể chuyện", "than thở", "tập nói", "đọc thơ", "kể chuyện xưa"]),
    ],
}

CITIES = ["Hà Nội", "Sài Gòn", "Đà Nẵng", "Huế", "Hội An", "Cần Thơ", "Đà Lạt", "Hải Phòng", "Nha Trang", "Vũng Tàu"]
WHEN = ["cuối tuần", "buổi tối", "sáng sớm", "ngày lễ", "giờ nghỉ trưa", "", "", ""]
EXTRA = ["cho người mới", "cho gia đình", "cho khách nước ngoài", "cho sinh viên", "cho người lớn tuổi", "theo nhóm nhỏ", "một kèm một", "", "", ""]
CREATORS = [f"@{name}{i}" for i in range(1, 400) for name in ("lan", "minh", "huy")][:900]


def fold(text):
    """Text without Vietnamese diacritics, as an agent might type it."""
    text = text.replace("đ", "d").replace("Đ", "D")
    return "".join(c for c in unicodedata.normalize("NFD", text) if unicodedata.category(c) != "Mn")


def title_of(verb, obj, city, when, extra):
    parts = [verb, obj, extra, "ở " + city, when]
    text = " ".join(p for p in parts if p)
    return text[0].upper() + text[1:]


def item_markdown(item):
    tags = ", ".join(item["tags"])
    return (
        "---\n"
        f"id: {item['code']}\n"
        f"type: {item['kind']}\n"
        f"status: {item['status']}\n"
        f"tags: [{tags}]\n"
        f"summary: {item['summary']}\n"
        "---\n"
        f"# {item['title']}\n\n"
        f"Group: {item['group']}. Creator: {item['creator']}. "
        f"Created {item['created']}, updated {item['updated']}. "
        f"Link: https://example.test/{item['kind']}s/{item['slug']}\n"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("out", help="the directory to write (must not exist)")
    parser.add_argument("--items", type=int, default=10000)
    parser.add_argument("--questions", type=int, default=200)
    parser.add_argument("--seed", type=int, default=7)
    args = parser.parse_args()
    if os.path.exists(args.out):
        parser.error(f"{args.out} exists")
    rng = random.Random(args.seed)

    families = [(group, fam) for group, fams in GROUPS.items() for fam in fams]
    start = datetime.date(2025, 1, 1)
    items, taken, clusters = [], set(), []

    def new_item(kind, group, verb, obj, city, when, extra):
        n = len(items)
        prefix = "SVC" if kind == "service" else "PRP"
        code = f"{prefix}-{n:05d}"
        created = start + datetime.timedelta(days=rng.randrange(600))
        updated = created + datetime.timedelta(days=rng.randrange(60))
        title = title_of(verb, obj, city, when, extra)
        status = rng.choice(["approved", "approved", "deprecated"]) if kind == "service" else rng.choice(
            ["review", "review", "approved", "deprecated"]
        )
        item = {
            "code": code,
            "kind": kind,
            "group": group,
            "status": status,
            "title": title,
            "slug": code.lower(),
            "summary": f"{verb.capitalize()} {obj} {('(' + extra + ')') if extra else ''} tại {city}.".replace("  ", " "),
            "tags": sorted({group, city.lower().replace(" ", "-"), obj.split()[0].lower()}),
            "creator": rng.choice(CREATORS),
            "created": created.isoformat(),
            "updated": updated.isoformat(),
            "path": f"{kind}s/{code.lower()}.md",
        }
        items.append(item)
        return item

    # Clusters first: an original and one or two rewordings by other creators.
    while len(clusters) < args.questions:
        group, (verb, synonyms, objects) = rng.choice(families)
        obj, city = rng.choice(objects), rng.choice(CITIES)
        when, extra = rng.choice(WHEN), rng.choice(EXTRA)
        if (verb, obj, city) in taken:
            continue
        taken.add((verb, obj, city))
        words = synonyms[:]
        rng.shuffle(words)
        members = [new_item("service", group, verb, obj, city, when, extra)]
        for synonym in words[: rng.choice([1, 2])]:
            members.append(new_item("proposal", group, synonym, obj, city, rng.choice(WHEN), extra))
        query_verb = rng.choice(synonyms)
        query = f"{query_verb} {obj} {city}"
        if rng.random() < 1 / 3:
            query = fold(query)
        clusters.append((query, [m["path"][:-3] for m in members], members[0]["title"]))

    # Then the rest of the catalogue: other activities, objects or cities than any
    # cluster's, so each cluster is exactly the duplicates of its question.
    while len(items) < args.items:
        group, (verb, synonyms, objects) = rng.choice(families)
        obj, city = rng.choice(objects), rng.choice(CITIES)
        when, extra = rng.choice(WHEN), rng.choice(EXTRA)
        if (verb, obj, city) in taken:
            continue
        kind = "service" if rng.random() < 0.4 else "proposal"
        new_item(kind, group, rng.choice(synonyms) if kind == "proposal" else verb, obj, city, when, extra)

    os.makedirs(os.path.join(args.out, ".kiwi"))
    os.makedirs(os.path.join(args.out, "bench"))
    for kind in ("services", "proposals"):
        os.makedirs(os.path.join(args.out, kind))
    for item in items:
        with open(os.path.join(args.out, item["path"]), "w", encoding="utf-8") as f:
            f.write(item_markdown(item))
    files = {
        ".kiwi/kiwi.toml": 'namespace = "catalogue"\n\n[plugins.vec]\nenabled = true\n',
        ".kiwi/policy.cedar": (
            "// Every staff member reads the catalogue.\n"
            'permit (principal in Kiwi::Group::"staff", action == Kiwi::Action::"read", resource);\n'
            "// Local plugins index it.\n"
            'permit (principal, action == Kiwi::Action::"read", resource)\n'
            'when { context.agent == "plugin" && context.channel == "local" };\n'
        ),
        ".kiwi/principals.yaml": (
            "users:\n  - id: agent\n    name: Agent\n    groups: [staff]\n"
            "agents:\n  - id: bench\n  - id: cli\n"
            "default_agent:\n  labels: []\n"
        ),
        "KIWIOWNERS": "*  label=internal  owner=@catalogue\n",
        "README.md": "# Catalogue\n\nOne record per service and proposal, generated by bench/catalogue.py.\n",
        "bench/principals.yaml": (
            "principals:\n  - id: agent\n    agent: bench\n"
            f"    must_see: [\"{clusters[0][1][0]}\"]\n"
        ),
    }
    questions = ["questions:"]
    for i, (query, answers, title) in enumerate(clusters):
        questions.append(f"  - id: c{i:03d}")
        questions.append(f"    question: {quote(title)}")
        questions.append(f"    queries: [{quote(query)}]")
        questions.append("    principal: agent")
        questions.append("    answers: [" + ", ".join(quote(a) for a in answers) + "]")
    files["bench/questions.yaml"] = "\n".join(questions) + "\n"
    for path, text in files.items():
        with open(os.path.join(args.out, path), "w", encoding="utf-8") as f:
            f.write(text)
    env = dict(os.environ, GIT_AUTHOR_DATE="2026-01-01T00:00:00Z", GIT_COMMITTER_DATE="2026-01-01T00:00:00Z")
    git = ["git", "-C", args.out, "-c", "user.name=catalogue", "-c", "user.email=catalogue@example.test"]
    subprocess.run(git[:3] + ["init", "--quiet", "--initial-branch=main"], check=True)
    subprocess.run(git + ["add", "-A"], check=True)
    subprocess.run(git + ["commit", "--quiet", "-m", "catalogue"], check=True, env=env)
    print(f"{len(items)} items, {len(clusters)} questions in {args.out}")


def quote(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


if __name__ == "__main__":
    main()
