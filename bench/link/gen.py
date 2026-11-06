"""Generate a synthetic C++ program for link benchmarks (bench/link/run.sh).

Each translation unit instantiates the same standard containers (duplicate
COMDATs the linker folds) plus its own types and templates (unique symbols),
compiled with debug info, so the link and its PDB are big. Usage:
    python bench/link/gen.py [num_tus]
"""
import pathlib
import shutil
import sys

n = int(sys.argv[1]) if len(sys.argv) > 1 else 1000
out = pathlib.Path(__file__).parent / "gen"
shutil.rmtree(out, ignore_errors=True)
out.mkdir()

TU = """#include <algorithm>
#include <functional>
#include <map>
#include <memory>
#include <numeric>
#include <string>
#include <unordered_map>
#include <vector>

namespace bench_{i} {{
struct Item {{
    int id;
    std::string name;
    double weight;
}};

template <int K>
int work(std::vector<Item>& items) {{
    std::sort(items.begin(), items.end(), [](const Item& a, const Item& b) {{
        return (a.id * (K + 1)) % 97 < (b.id * (K + 1)) % 97;
    }});
    std::map<std::string, int> by_name;
    for (const Item& it : items) by_name[it.name] += it.id + K;
    std::unordered_map<int, std::string> by_id;
    for (const Item& it : items) by_id.emplace(it.id, it.name);
    auto shared = std::make_shared<std::vector<int>>(items.size(), K);
    std::function<int(int)> f = [&](int x) {{ return x + (int)by_id.size() + (*shared)[0]; }};
    return std::accumulate(by_name.begin(), by_name.end(), f(K),
        [](int acc, const auto& kv) {{ return acc + kv.second + (int)kv.first.size(); }});
}}
}}  // namespace bench_{i}

int bench_entry_{i}(int seed) {{
    std::vector<bench_{i}::Item> items;
    for (int j = 0; j < 16; ++j)
        items.push_back({{seed + j * {i}, "item" + std::to_string(j), j * 0.5}});
    return bench_{i}::work<0>(items) + bench_{i}::work<1>(items) + bench_{i}::work<2>(items)
         + bench_{i}::work<3>(items) + bench_{i}::work<4>(items) + bench_{i}::work<5>(items);
}}
"""

for i in range(n):
    (out / f"tu_{i:05}.cpp").write_text(TU.format(i=i))

decls = "".join(f"int bench_entry_{i}(int);\n" for i in range(n))
calls = "".join(f"    sum += bench_entry_{i}(argc);\n" for i in range(n))
(out / "main.cpp").write_text(f"""#include <stdio.h>
{decls}
int main(int argc, char**) {{
    long long sum = 0;
{calls}    printf("checksum %lld\\n", sum);
    return 0;
}}
""")

(out / "ANUBIS").write_text("""cc_binary(
    name = "main",
    lang = "cpp",
    srcs = glob(["*.cpp"]),
)
""")
print(f"wrote {n} translation units to {out}")
