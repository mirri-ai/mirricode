//! 问询问题项的 id 铸造 —— `AskUserQuestion` 工具入参与
//! `event.question.requested` / `session.snapshot.pending_questions`
//! 两条 wire 面共用的单一实现。
//!
//! 工具的入参天然没有 id（模型只给 `question`/`header`/`options[].label`），
//! 而两个消费面都要求 item 与 option 可寻址：item id 是客户端答案映射表的
//! key，option id 决定作答回传的字符串。两处各自铸造必然漂移，所以收敛到
//! 这里。

use serde_json::{Value, json};

/// 为问题项补齐客户端可寻址的 id，并注入 `allowOther: true`。
///
/// option 的 id 取 `label` 本身：rust 的 `question.answer` 把答案字符串原样
/// 交给 gate（没有 TS 侧那种 id→label 反查层），所以"id 就是 label"是唯一
/// 能让模型收到的工具结果保持可读的形式。item id 用 `q_<idx>`（只作答案映射
/// 表的 key，永不进入模型可见文本）；空 label 退化为 `opt_<item>_<opt>`。
///
/// 每个 item 一并带上 klient 行的 `allowOther: true`（工具对模型的描述承诺
/// "用户永远有 Other 选项"）——快照恢复的卡片因此拥有与直播卡片一致的自由
/// 文本输入。非对象项与非数组输入原样返回（诚实透传，不 panic）。
pub fn mint_question_item_ids(questions: &Value) -> Value {
    let Some(items) = questions.as_array() else {
        return questions.clone();
    };
    Value::Array(
        items
            .iter()
            .enumerate()
            .map(|(item_idx, item)| {
                let Some(obj) = item.as_object() else {
                    return item.clone();
                };
                let mut out = obj.clone();
                out.insert("allowOther".to_string(), Value::Bool(true));
                if out
                    .get("id")
                    .is_none_or(|id| id.as_str().is_none_or(|s| s.is_empty()))
                {
                    out.insert("id".to_string(), json!(format!("q_{item_idx}")));
                }
                if let Some(options) = out.get("options").and_then(Value::as_array) {
                    let minted: Vec<Value> = options
                        .iter()
                        .enumerate()
                        .map(|(opt_idx, option)| {
                            let Some(option_obj) = option.as_object() else {
                                return option.clone();
                            };
                            let mut option_out = option_obj.clone();
                            if option_out
                                .get("id")
                                .is_none_or(|id| id.as_str().is_none_or(|s| s.is_empty()))
                            {
                                let label = option_out
                                    .get("label")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                let id = if label.is_empty() {
                                    format!("opt_{item_idx}_{opt_idx}")
                                } else {
                                    label.to_string()
                                };
                                option_out.insert("id".to_string(), json!(id));
                            }
                            Value::Object(option_out)
                        })
                        .collect();
                    out.insert("options".to_string(), Value::Array(minted));
                }
                Value::Object(out)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_mint_item_and_option_ids_when_items_carry_none() {
        // Given raw tool input with two options and no ids anywhere
        let questions = json!([{
            "question": "Which language?",
            "options": [{ "label": "Rust" }, { "label": "TypeScript" }]
        }]);

        // When minting
        let minted = mint_question_item_ids(&questions);

        // Then the item and every option are addressable, and the answer
        // string the option id resolves to is the label itself
        assert_eq!(minted[0]["id"], json!("q_0"));
        assert_eq!(minted[0]["allowOther"], json!(true));
        assert_eq!(minted[0]["options"][0]["id"], json!("Rust"));
        assert_eq!(minted[0]["options"][1]["id"], json!("TypeScript"));
    }

    #[test]
    fn should_keep_existing_ids_when_items_already_carry_them() {
        // Given items whose ids were minted by another surface
        let questions = json!([{
            "id": "q_7",
            "question": "Pick",
            "options": [{ "id": "opt_7_0", "label": "A" }, { "id": "opt_7_1", "label": "B" }]
        }]);

        // When minting
        let minted = mint_question_item_ids(&questions);

        // Then the supplied ids survive untouched
        assert_eq!(minted[0]["id"], json!("q_7"));
        assert_eq!(minted[0]["options"][0]["id"], json!("opt_7_0"));
    }

    #[test]
    fn should_fall_back_to_positional_id_when_option_label_is_empty() {
        // Given an option with an empty label
        let questions = json!([{ "question": "Pick", "options": [{ "label": "" }] }]);

        // When minting
        let minted = mint_question_item_ids(&questions);

        // Then the option still gets a unique id
        assert_eq!(minted[0]["options"][0]["id"], json!("opt_0_0"));
    }

    #[test]
    fn should_pass_through_when_input_is_not_an_array() {
        // Given a non-array value (defensive)
        let questions = json!({ "question": "Pick" });

        // When minting
        let minted = mint_question_item_ids(&questions);

        // Then it comes back unchanged
        assert_eq!(minted, questions);
    }
}
