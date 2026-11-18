// 用真正的 C# 实现（.NET 10 + 参考源码 PinyinSearch.cs）导出拼音表与行为语料，供 Rust 版对照。
// 用法：refdump.exe <表输出目录> <语料输出文件>
//   refdump.exe ../../tools ../../crates/snug-core/src/pinyin/corpus.tsv
// 产出：
//   <表输出目录>/pinyin_ref_dump.txt —— "TABLE <hex> <full> <initials>" 每行一个字（只列与"原样小写"不同的字）
//   <语料输出文件>                     —— 文本/查询 -> Matches/Score/SortKey/ToPinyin 的真值
using System.Text;
using SnugWin.Core.PhoneScreen;

string dumpDir = args.Length > 0 ? args[0] : ".";
string corpusPath = args.Length > 1 ? args[1] : Path.Combine(dumpDir, "pinyin_corpus.tsv");
var utf8 = new UTF8Encoding(false);

// ---- 1. GB2312 映射 ----
var sb = new StringBuilder();
int count = 0;
for (int cp = 0x80; cp <= 0xFFFF; cp++)
{
    if (cp >= 0xD800 && cp <= 0xDFFF) continue;
    char ch = (char)cp;
    var (full, initials) = PinyinSearch.ToPinyin(ch.ToString());
    string fallback = char.ToLowerInvariant(ch).ToString();
    if (full == fallback && initials == fallback) continue;
    count++;
    sb.Append("TABLE ").Append(cp.ToString("X4")).Append(' ').Append(full).Append(' ').Append(initials).Append('\n');
}
sb.Append("COUNT ").Append(count).Append('\n');
File.WriteAllText(Path.Combine(dumpDir, "pinyin_ref_dump.txt"), sb.ToString(), utf8);

// ---- 2. 行为语料 ----
string[] texts =
[
    "微信", "备忘录", "记事本", "终端", "Visual Studio Code", "Visual Studio", "网易云音乐",
    "淘宝", "闲鱼", "拼多多", "哔哩哔哩", "钉钉", "剪映", "豆瓣", "多邻国", "知乎",
    "重庆银行", "长按", "重复", "银行", "音乐", "调整", "传奇", "藏区", "微信读书",
    "高德地图", "支付宝", "抖音", "QQ音乐", "WPS Office", "Microsoft Edge", "设置",
    "计算器", "画图", "相机", "文件资源管理器", "微信输入法", "中国", "人", "一", "阿", "㆒",
    " 微 信 ", "微　信", "", " ", "　", "Visual Studio Code ", "task manager", "SnugWin", "重", "乐", "行", "长", "重庆",
];
string[] queries =
[
    "", " ", "　", "wx", "WX", "weixin", "wei xin", "bwl", "beiwanglu", "zd", "cq", "chongqing",
    "zhongqing", "zq", "yy", "yinyue", "yhy", "yinhang", "ch", "chang", "zhang", "vscode",
    "studio", "vs code", "vs", "code", "QQ", "qq", "wps", "edge", "z", "a", "#", "中国",
    "zhongguo", "zg", "r", "ren", "yi", "一", "microsoft", "office", "设置", "shezhi", "sz",
    "ji", "jy", "db", "duo", "xy", "pdd", "blbl", "bilibili", "ding", "dd", "2", "重", "chong",
];

var corpus = new StringBuilder();
foreach (var text in texts)
{
    var (full, initials) = PinyinSearch.ToPinyin(text);
    string sortKey = PinyinSearch.SortKey(text);
    corpus.Append("T\t").Append(text).Append('\t').Append(full).Append('\t').Append(initials).Append('\t').Append(sortKey).Append('\n');
    foreach (var q in queries)
    {
        corpus.Append("Q\t").Append(text).Append('\t').Append(q).Append('\t')
              .Append(PinyinSearch.Matches(text, q) ? '1' : '0').Append('\t')
              .Append(PinyinSearch.Score(text, q)).Append('\n');
    }
}

File.WriteAllText(corpusPath, corpus.ToString(), utf8);

Console.WriteLine($"表 {count} 字，语料 {texts.Length} 文本 x {queries.Length} 查询");
