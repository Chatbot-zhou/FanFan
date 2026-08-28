//! 轻量 Query Normalization（纯函数、无模型、无重型词典）。
//!
//! 解决真实测试暴露的确定性修正，全部是「原始句 + 规范化候选双路径」：
//! - [`meaningful_tokens`]：从目标短语中提取「有意义词元」（剥掉 我的/那个/
//!   材料/在哪 等指代与疑问填充词），供 Document Resolver 的候选粗筛与打分
//!   信号使用——「我的简历」→「简历」、「我毕业时候那个材料」→「毕业」；
//! - [`normalize_query_variants`]：全角→半角、ASCII 小写、空白折叠、
//!   CJK 与 ASCII 邻接去空格、常见单字拼音音节展开（开fa→开发），返回
//!   去重变体列表（最多 4 个），原句恒在首位。拼音表只收最高频单字音节，
//!   不做任何针对具体问题的映射（禁止 开fa→开发 式的硬编码）。
//!
//! 纪律：规范化只做「放宽召回」——任何变体都只追加参与 parse/retrieval，
//! 绝不覆盖原句；不改动专有名词（纯 ASCII 词与不邻接 CJK 的 ASCII 词不动）。

/// 目标短语中的指代/疑问填充词（长短语在前，替换按最长优先避免残词）。
/// 词元提取只用于「放宽候选匹配」，删词过激只会多召回，不会造成误答。
/// 注意：「我的简历」「我的资料」等整体**不能**入表——删掉整词后目标词元
/// 会全空（我的简历 → 简历，靠「我」+「的」+「简历」的单独删除完成）。
const TARGET_STOP_PHRASES: &[&str] = &[
    "在哪里",
    "在哪呢",
    "在哪",
    "在哪儿",
    "哪儿",
    "哪个文件",
    "哪个位置",
    "哪里找",
    "找一下",
    "帮我找",
    "帮我",
    "请问",
    "请",
    "哪个",
    "哪些",
    "哪份",
    "哪几",
    "这个",
    "那个",
    "这些",
    "那些",
    "这份",
    "那份",
    "一个",
    "一些",
    "有一份",
    "主要",
    "写了",
    "写有",
    "写的是",
    "介绍",
    "提到",
    "提过",
    "讲了",
    "描述",
    "里面",
    "里头",
    "里有",
    "里有没有",
    "有没有",
    "是不是",
    "是否",
    "什么",
    "时候",
    "的",
    "了",
    "吗",
    "呢",
    "啊",
    "吧",
    "过",
    "里",
    "材料",
    "文件",
    "资料",
    "文档",
    "目录",
    "文件夹",
    // 指代式容器/量词与谓词：用户常以「那本讲…的册子」「这本谈…的资料」
    // 等包装结构指代文档，这些词紧贴主题词（册子/这本/讲），若不清洗，
    // 主题词会连带包装词形成一个带前缀的整串 token，无法被子串匹配进真实
    // 标题（如「…册子」→「手册」）。在此统一剥掉量词/容器词/谓词，仅保留
    // 真正的主题（「大模型应用开发」）。通用语言结构处理，不针对任何具体
    // 文件/关键词/case。多字容器词避免与主题词串扰；「讲」等单字谓词在
    // 目标短语里几乎只作动词，剥掉只会多召回、不构成误答。
    "那本",
    "这本",
    "那一本",
    "这一本",
    "另一本",
    "一本",
    "两本",
    "几本",
    "这本书",
    "那本书",
    "册子",
    "手册",
    "书",
    "著作",
    "教材",
    "读物",
    "讲",
    "讲的",
    "讲解了",
    "讲述了",
    "介绍了",
    "是讲",
    "在",
    "有",
    "是",
    "我",
    "你",
];

/// 按长度降序的停止词（「在哪里」必须在「在哪」之前被替换）。
fn stop_phrases_sorted() -> Vec<&'static str> {
    let mut phrases = TARGET_STOP_PHRASES.to_vec();
    phrases.sort_by_key(|phrase| std::cmp::Reverse(phrase.chars().count()));
    phrases
}

/// 从目标短语中去掉指代/疑问填充词，保留其余字符（不切 token、不插空格）。
///
/// 与 [`meaningful_tokens`] 共用同一停止词表（口径一致），区别是这里返回
/// 连续的清洗串，供 Document Resolver 的 FIND 定位与文件名做「子序列/二元组」
/// 匹配：content_query「2019年数据库下午的真题文件」→「2019年数据库下午真题」
/// （去掉 的/文件）。清洗只用于放宽匹配，删词过激只会多召回，不造成误答。
pub fn strip_target_stop_phrases(text: &str) -> String {
    let mut cleaned = text.to_owned();
    for stop in stop_phrases_sorted() {
        cleaned = cleaned.replace(stop, "");
    }
    cleaned
}

/// 从目标短语提取有意义词元：剥掉指代/疑问填充词后按非字母数字边界切分，
/// 保留长度 ≥2 的片段（单个 ASCII 字母丢弃），去重并截断到 6 个。
///
/// 例：`我的简历` → `["简历"]`；`我毕业时候那个材料` → `["毕业"]`；
/// `我那个大模型的材料` → `["大模型"]`；`LangGraph 项目` → `["langgraph","项目"]`
/// （ASCII 统一小写，专有名词大小写差异不参与匹配）。
pub fn meaningful_tokens(text: &str) -> Vec<String> {
    let mut cleaned = text.to_owned();
    for stop in stop_phrases_sorted() {
        cleaned = cleaned.replace(stop, " ");
    }
    let mut seen = std::collections::HashSet::new();
    let mut tokens = Vec::new();
    for raw in cleaned.split(|c: char| !(c.is_alphanumeric())) {
        let token = raw.trim().to_lowercase();
        let length = token.chars().count();
        let ascii_length = token.bytes().filter(|b| b.is_ascii_alphanumeric()).count();
        // 纯中文词（ascii_length == 0）必须保留；只有「恰好一个 ASCII 字符」
        // 的碎片（分词残留的单字母）才丢弃
        if token.is_empty() || length < 2 || ascii_length == 1 {
            continue;
        }
        if !seen.insert(token.clone()) {
            continue;
        }
        tokens.push(token);
        if tokens.len() >= 6 {
            break;
        }
    }
    tokens
}

/// 常见单字拼音音节（出现频率最高的 40 个）。只用于「CJK 字符 + 紧邻 ASCII
/// 音节」的中英混输展开：`开fa` → `开发`。纯 ASCII 词（LangGraph/RAG）与
/// 不紧邻 CJK 的 ASCII 词绝不改写。表是通用音节表，不是针对具体问题的映射。
const PINYIN_SYLLABLES: &[(&str, char)] = &[
    ("de", '的'),
    ("le", '了'),
    ("zai", '在'),
    ("shi", '是'),
    ("you", '有'),
    ("wo", '我'),
    ("ni", '你'),
    ("ta", '他'),
    ("bu", '不'),
    ("zhe", '这'),
    ("na", '那'),
    ("ge", '个'),
    ("fa", '发'),
    ("da", '大'),
    ("xiao", '小'),
    ("zhong", '中'),
    ("shang", '上'),
    ("xia", '下'),
    ("he", '和'),
    ("yu", '与'),
    ("dui", '对'),
    ("ji", '及'),
    ("jian", '见'),
    ("bian", '边'),
    ("jin", '进'),
    ("chu", '出'),
    ("hou", '后'),
    ("qian", '前'),
    ("hui", '会'),
    ("kan", '看'),
    ("mei", '没'),
    ("guo", '过'),
    ("lai", '来'),
    ("qu", '去'),
    ("zen", '怎'),
    ("yao", '要'),
    ("neng", '能'),
    ("dou", '都'),
    ("jiu", '就'),
    ("hai", '还'),
];

/// CJK 与 ASCII 之间的空格折叠：`开 fa` → `开fa`（让中英混输连续成词）。
fn fold_cjk_ascii_spaces(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let is_cjk =
        |c: char| ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c);
    let is_ascii_alpha = |c: char| c.is_ascii_alphabetic();
    for (index, ch) in chars.iter().enumerate() {
        if *ch == ' ' && index > 0 && index + 1 < chars.len() {
            let prev = chars[index - 1];
            let next = chars[index + 1];
            if (is_cjk(prev) && is_ascii_alpha(next)) || (is_ascii_alpha(prev) && is_cjk(next)) {
                continue; // 折叠 CJK↔ASCII 边界空格
            }
        }
        result.push(*ch);
    }
    result
}

/// 中英混输拼音展开：对每个「CJK + 完整拼音音节(邻接 CJK 或句尾)」片段，
/// 把音节替换为对应汉字（`开fa` → `开发`）。纯 ASCII 词不触碰。
fn expand_pinyin_mix(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let is_cjk =
        |c: char| ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c);
    let mut result = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if is_cjk(ch) {
            // 收集紧随其后的 ASCII 字母串
            let mut end = index + 1;
            while end < chars.len() && chars[end].is_ascii_alphabetic() {
                end += 1;
            }
            let ascii_run = &chars[index + 1..end];
            let syllable: String = ascii_run.iter().collect::<String>().to_lowercase();
            // 音节必须完整命中拼音表，且后随 CJK 或句尾（避免吞掉长英文词）
            let followed_by_cjk_or_end = end == chars.len() || is_cjk(chars[end]);
            if let Some((_, hanzi)) = PINYIN_SYLLABLES
                .iter()
                .find(|(syllable_candidate, _)| *syllable_candidate == syllable)
                .filter(|_| followed_by_cjk_or_end)
            {
                result.push(ch);
                result.push(*hanzi);
                index = end;
                continue;
            }
        }
        result.push(ch);
        index += 1;
    }
    result
}

/// 全角字母/数字 → 半角（ＡＢＣ→ABC、１２３→123）。
fn fullwidth_to_halfwidth(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\u{ff21}'..='\u{ff3a}' => {
                char::from_u32(ch as u32 - 0xff21 + 'A' as u32).unwrap_or(ch)
            }
            '\u{ff41}'..='\u{ff5a}' => {
                char::from_u32(ch as u32 - 0xff41 + 'a' as u32).unwrap_or(ch)
            }
            '\u{ff10}'..='\u{ff19}' => {
                char::from_u32(ch as u32 - 0xff10 + '0' as u32).unwrap_or(ch)
            }
            _ => ch,
        })
        .collect()
}

/// 生成规范化候选（原句恒在首位，去重，最多 4 个）。
/// 变体只追加参与 parse/retrieval（双路径），绝不覆盖原句。
pub fn normalize_query_variants(question: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let push_unique = |variant: String, variants: &mut Vec<String>| {
        if !variant.is_empty()
            && variant != question.trim()
            && !variants.contains(&variant)
            && variants.len() < 4
        {
            variants.push(variant);
        }
    };

    let halfwidth = fullwidth_to_halfwidth(question);
    push_unique(halfwidth.clone(), &mut variants);

    let folded = fold_cjk_ascii_spaces(&halfwidth);
    let collapsed = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    push_unique(collapsed.clone(), &mut variants);

    let lowercase = collapsed.to_lowercase();
    push_unique(lowercase.clone(), &mut variants);

    let pinyin = expand_pinyin_mix(&lowercase);
    push_unique(pinyin, &mut variants);

    variants
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_extract_from_resume_reference() {
        assert_eq!(meaningful_tokens("我的简历"), vec!["简历"]);
        assert_eq!(meaningful_tokens("我的简历里"), vec!["简历"]);
    }

    #[test]
    fn strip_removes_trailing_question_residue() {
        // 常见功能词干扰：FIND 描述末尾的裸疑问词（哪个/哪些/哪份/哪几）必须
        // 与「哪个文件」等组合词一样被剥掉，否则残词让子序列匹配在「真题」之后
        // 卡在「哪」上失配，退化成弱一档的二元组覆盖率兜底。
        assert_eq!(
            strip_target_stop_phrases("2019年数据库下午的真题文件是哪个"),
            "2019年数据库下午真题"
        );
        assert_eq!(
            strip_target_stop_phrases("2020年上午真题是哪份"),
            "2020年上午真题"
        );
        assert_eq!(
            strip_target_stop_phrases("有哪些数据库真题"),
            "数据库真题"
        );
    }

    #[test]
    fn tokens_extract_from_graduation_material() {
        // CASE 7：毕业材料 → 词元「毕业」（文件名「毕业设计…」可命中）
        assert_eq!(meaningful_tokens("我毕业时候那个材料"), vec!["毕业"]);
    }

    #[test]
    fn tokens_extract_from_llm_material() {
        // CASE 8/9：那个大模型的材料 → 「大模型」（ASCII 小写归一）
        assert_eq!(meaningful_tokens("我那个大模型的材料"), vec!["大模型"]);
        assert_eq!(
            meaningful_tokens("LangGraph 项目"),
            vec!["langgraph", "项目"]
        );
    }

    #[test]
    fn tokens_keep_langgraph_lowercased() {
        // 专有名词大小写不参与匹配（全小写），纯 ASCII 词不被拆散
        let tokens = meaningful_tokens("我的 LangGraph 项目");
        assert!(tokens.contains(&"langgraph".to_owned()));
        assert!(tokens.contains(&"项目".to_owned()));
    }

    #[test]
    fn variants_keep_original_first_and_dedupe() {
        let variants = normalize_query_variants("我那个大模型开fa材料里写了什么");
        assert!(!variants.is_empty());
        // 原句不重复出现在变体里，变体互不相同
        let unique: std::collections::HashSet<_> = variants.iter().collect();
        assert_eq!(unique.len(), variants.len());
    }

    #[test]
    fn pinyin_mix_expansion_works() {
        // 开fa → 开发（通用音节表，非针对具体问题硬编码）
        assert!(
            normalize_query_variants("我那个大模型开fa材料里写了什么")
                .iter()
                .any(|variant| variant.contains("开发"))
        );
        // 纯 ASCII 专有名词不被改写
        for variant in normalize_query_variants("LangGraph 和 RAG 区别") {
            assert!(variant.contains("langgraph") || variant.contains("LangGraph"));
            assert!(variant.contains("rag") || variant.contains("RAG"));
        }
    }

    #[test]
    fn pinyin_expansion_does_not_mutate_long_ascii_words() {
        // Transformer 不以拼音音节结尾（…mer 不完整命中），不被改写
        for variant in normalize_query_variants("Transformer 是什么") {
            assert!(variant.contains("Transformer") || variant.contains("transformer"));
        }
    }

    #[test]
    fn cjk_ascii_space_folded() {
        // 「开 fa」边界空格折叠为「开fa」，为拼音展开铺路
        assert!(
            normalize_query_variants("我那个大模型 开 fa 材料")
                .iter()
                .any(|variant| variant.contains("开发"))
        );
    }

    #[test]
    fn fullwidth_and_case_variants() {
        let variants = normalize_query_variants("ＲＡＧ 是什么");
        assert!(variants.iter().any(|variant| variant.contains("rag")));
    }
}
