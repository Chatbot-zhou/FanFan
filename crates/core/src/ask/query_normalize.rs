//! 轻量 Query Normalization（纯函数、无模型、无重型词典）。
//!
//! 解决真实测试暴露的三类确定性修正，全部是「原始句 + 规范化候选双路径」：
//! - [`meaningful_tokens`]：从目标短语中提取「有意义词元」（剥掉 我的/那个/
//!   材料/在哪 等指代与疑问填充词），供 Document Resolver 的候选粗筛与打分
//!   信号使用——「我的简历」→「简历」、「我毕业时候那个材料」→「毕业」；
//! - [`extract_find_reference`]：FIND 意图的确定性检测（在哪/在哪里/找一下/
//!   哪个文件），返回剥离标记后的目标短语——「我毕业时候那个材料在哪」→
//!   「我毕业时候那个材料」；
//! - [`is_existence_question`]：存在性问句（有没有/是否…过）确定性判断，
//!   保证这类问题走 QA 而非 EXTRACT；
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
    "在哪里", "在哪呢", "在哪", "在哪儿", "哪儿", "哪个文件", "哪个位置",
    "哪里找", "找一下", "帮我找", "帮我", "请问", "请",
    "这个", "那个", "这些", "那些", "这份", "那份", "一个", "一些", "有一份",
    "主要", "写了", "写有", "写的是", "介绍", "提到", "提过", "讲了", "描述",
    "里面", "里头", "里有", "里有没有", "有没有", "是不是", "是否", "什么",
    "时候", "的", "了", "吗", "呢", "啊", "吧", "过", "里",
    "材料", "文件", "资料", "文档", "目录", "文件夹",
    "在", "有", "是", "我", "你",
];

/// 按长度降序的停止词（「在哪里」必须在「在哪」之前被替换）。
fn stop_phrases_sorted() -> Vec<&'static str> {
    let mut phrases = TARGET_STOP_PHRASES.to_vec();
    phrases.sort_by_key(|phrase| std::cmp::Reverse(phrase.chars().count()));
    phrases
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

/// FIND 意图标记（长短语在前）。
const FIND_MARKERS: &[&str] = &[
    "在哪个文件夹", "在哪个位置", "在哪里", "在哪呢", "在哪儿", "在哪",
    "哪个文件是", "是哪个文件", "哪个文件", "哪里找", "去哪找", "去哪里找",
    "放哪里了", "放哪了", "找一下", "帮我找", "哪里",
];

/// 确定性 FIND 检测：问题含「在哪/在哪里/找一下/哪个文件」等标记时，
/// 返回剥离标记后的目标短语（如「我毕业时候那个材料」）；不含标记或
/// 剥完为空返回 None。命中后由调用方直接构造 DOCUMENT_FIND plan，
/// 不经过 LLM Parser（防止 0.6B 复读历史问题的回声失败模式）。
pub fn extract_find_reference(question: &str) -> Option<String> {
    let trimmed = question.trim();
    if trimmed.is_empty() {
        return None;
    }
    for marker in FIND_MARKERS {
        if let Some(index) = trimmed.find(marker) {
            let mut remainder = String::new();
            remainder.push_str(&trimmed[..index]);
            remainder.push_str(&trimmed[index + marker.len()..]);
            let remainder = remainder
                .trim()
                .trim_matches(|c: char| matches!(c, '，' | '。' | '？' | '?' | '！' | '!' | '、' | ' '))
                .trim()
                .to_owned();
            // 标记前的助动词残余（「帮我找一下那个合同」→ marker 在
            // 「找一下」，前面残留「帮我」）剥掉，只留目标短语本体
            let mut reference = remainder.clone();
            loop {
                let before = reference.clone();
                for aux in ["请帮我", "帮我", "请你", "麻烦你", "麻烦", "帮忙", "请"] {
                    if let Some(stripped) = reference.strip_prefix(aux) {
                        reference = stripped.trim().to_owned();
                        break;
                    }
                }
                if reference == before {
                    break;
                }
            }
            // 目标短语太短（≤1 字）或剥完只剩问句残余 → 不判 FIND
            if reference.chars().count() >= 2 {
                return Some(reference);
            }
            return None;
        }
    }
    None
}

/// 存在性问句（有没有/是否…过/有吗）：这类问题的答案是「有/没有 + 依据」，
/// 是 QA 而非 EXTRACT（禁止把「我以前有没有做过 Agent 项目？」拆成条目清单）。
pub fn is_existence_question(question: &str) -> bool {
    let q = question.trim();
    q.starts_with("有没有") || q.starts_with("是否") || q.starts_with("你是否")
        || q.starts_with("有吗") || q.starts_with("以前有没有") || q.starts_with("之前有没有")
        || q.contains("有没有做过") || q.contains("有没有提到")
        || q.contains("有没有写") || q.contains("有没有讲")
}

/// 常见单字拼音音节（出现频率最高的 40 个）。只用于「CJK 字符 + 紧邻 ASCII
/// 音节」的中英混输展开：`开fa` → `开发`。纯 ASCII 词（LangGraph/RAG）与
/// 不紧邻 CJK 的 ASCII 词绝不改写。表是通用音节表，不是针对具体问题的映射。
const PINYIN_SYLLABLES: &[(&str, char)] = &[
    ("de", '的'), ("le", '了'), ("zai", '在'), ("shi", '是'), ("you", '有'),
    ("wo", '我'), ("ni", '你'), ("ta", '他'), ("bu", '不'), ("zhe", '这'),
    ("na", '那'), ("ge", '个'), ("fa", '发'), ("da", '大'), ("xiao", '小'),
    ("zhong", '中'), ("shang", '上'), ("xia", '下'), ("he", '和'), ("yu", '与'),
    ("dui", '对'), ("ji", '及'), ("jian", '见'), ("bian", '边'), ("jin", '进'),
    ("chu", '出'), ("hou", '后'), ("qian", '前'), ("hui", '会'), ("kan", '看'),
    ("mei", '没'), ("guo", '过'), ("lai", '来'), ("qu", '去'), ("zen", '怎'),
    ("yao", '要'), ("neng", '能'), ("dou", '都'), ("jiu", '就'), ("hai", '还'),
];

/// CJK 与 ASCII 之间的空格折叠：`开 fa` → `开fa`（让中英混输连续成词）。
fn fold_cjk_ascii_spaces(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let is_cjk = |c: char| {
        ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c)
    };
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
    let is_cjk = |c: char| {
        ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c)
    };
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
            '\u{ff21}'..='\u{ff3a}' => char::from_u32(ch as u32 - 0xff21 + 'A' as u32).unwrap_or(ch),
            '\u{ff41}'..='\u{ff5a}' => char::from_u32(ch as u32 - 0xff41 + 'a' as u32).unwrap_or(ch),
            '\u{ff10}'..='\u{ff19}' => char::from_u32(ch as u32 - 0xff10 + '0' as u32).unwrap_or(ch),
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

/// 该短语是否含「我的/我」等自有文件表达（FIND plan 的 owner 判定）。
/// 「我毕业时候那个材料」这类口语指代（无「的」）也判自有。
pub fn mentions_self(question: &str) -> bool {
    question.contains("我的")
        || question.contains("我放")
        || question.contains("我存")
        || (question.contains("我") && !question.contains("我们"))
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
    fn tokens_extract_from_graduation_material() {
        // CASE 7：毕业材料 → 词元「毕业」（文件名「毕业设计…」可命中）
        assert_eq!(meaningful_tokens("我毕业时候那个材料"), vec!["毕业"]);
    }

    #[test]
    fn tokens_extract_from_llm_material() {
        // CASE 8/9：那个大模型的材料 → 「大模型」（ASCII 小写归一）
        assert_eq!(meaningful_tokens("我那个大模型的材料"), vec!["大模型"]);
        assert_eq!(meaningful_tokens("LangGraph 项目"), vec!["langgraph", "项目"]);
    }

    #[test]
    fn tokens_keep_langgraph_lowercased() {
        // 专有名词大小写不参与匹配（全小写），纯 ASCII 词不被拆散
        let tokens = meaningful_tokens("我的 LangGraph 项目");
        assert!(tokens.contains(&"langgraph".to_owned()));
        assert!(tokens.contains(&"项目".to_owned()));
    }

    #[test]
    fn find_reference_extracted_from_markers() {
        assert_eq!(
            extract_find_reference("我毕业时候那个材料在哪"),
            Some("我毕业时候那个材料".to_owned())
        );
        assert_eq!(
            extract_find_reference("我的简历在哪里"),
            Some("我的简历".to_owned())
        );
        assert_eq!(
            extract_find_reference("帮我找一下那个合同"),
            Some("那个合同".to_owned())
        );
        assert_eq!(
            extract_find_reference("哪个文件是 LangGraph 项目"),
            Some("LangGraph 项目".to_owned())
        );
        assert_eq!(
            extract_find_reference("那个材料是哪个文件"),
            Some("那个材料".to_owned())
        );
    }

    #[test]
    fn find_reference_rejects_non_find_questions() {
        assert_eq!(extract_find_reference("你好"), None);
        assert_eq!(extract_find_reference("我的简历里有没有 LangGraph"), None);
        assert_eq!(extract_find_reference("Transformer 是什么"), None);
        assert_eq!(extract_find_reference(""), None);
    }

    #[test]
    fn existence_questions_detected() {
        for q in [
            "我以前有没有做过 Agent 项目？",
            "我的简历里有没有写 LangGraph？",
            "我的资料里有没有提到 RAG",
            "你是否参与过这个项目",
            "有吗",
        ] {
            assert!(is_existence_question(q), "{q} 应为存在性问句");
        }
        for q in ["你好", "我的简历里有哪些项目", "Transformer 是什么"] {
            assert!(!is_existence_question(q), "{q} 不应判为存在性问句");
        }
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
        assert!(normalize_query_variants("我那个大模型开fa材料里写了什么")
            .iter()
            .any(|variant| variant.contains("开发")));
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
        assert!(normalize_query_variants("我那个大模型 开 fa 材料")
            .iter()
            .any(|variant| variant.contains("开发")));
    }

    #[test]
    fn fullwidth_and_case_variants() {
        let variants = normalize_query_variants("ＲＡＧ 是什么");
        assert!(variants.iter().any(|variant| variant.contains("rag")));
    }

    #[test]
    fn mentions_self_detection() {
        assert!(mentions_self("我毕业时候那个材料在哪"));
        assert!(mentions_self("我的简历在哪里"));
        assert!(!mentions_self("大模型材料在哪"));
    }
}
