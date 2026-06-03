use std::collections::HashMap;

use worker::kv::KvStore;

use crate::storage::{doc_key, kv_get_text};
use crate::types::Workflow;

pub const DEFAULT_TEMPLATE: &str = r#"First, read this README for project context:

```
{{readme}}
```

---

Here is the ORIGINAL specification for reference:

```
{{spec}}
```

---

Below is the current working version of the document to improve. On the first
round this will be identical to the original above. On subsequent rounds it
contains the improvements from the previous iteration:

{{previous_round}}

---

YOUR TASK: Produce a COMPLETE, improved version of the document above. Make it
better in terms of architecture, security, reliability, performance,
completeness, and clarity. Fix any errors, fill gaps, resolve contradictions,
and strengthen weak sections.

IMPORTANT: Output the ENTIRE revised document — not a summary, not a list of
suggestions, not diffs. The full document with all improvements applied.
Preserve the overall structure and formatting. Every section of the original
should appear in your output, improved where needed and unchanged where
already strong."#;

pub const DEFAULT_TEMPLATE_WITH_IMPL: &str = r#"First, read this README for project context:

```
{{readme}}
```

---

Here is the current implementation to keep in mind — the specification must
ultimately be translatable into code:

```
{{implementation}}
```

---

Here is the ORIGINAL specification for reference:

```
{{spec}}
```

---

Below is the current working version of the document to improve. On the first
round this will be identical to the original above. On subsequent rounds it
contains the improvements from the previous iteration:

{{previous_round}}

---

YOUR TASK: Produce a COMPLETE, improved version of the document above. Make it
better in terms of architecture, security, reliability, performance,
completeness, and clarity. Fix any errors, fill gaps, resolve contradictions,
and strengthen weak sections. Keep the implementation in mind — ensure the
specification remains implementable.

IMPORTANT: Output the ENTIRE revised document — not a summary, not a list of
suggestions, not diffs. The full document with all improvements applied.
Preserve the overall structure and formatting. Every section of the original
should appear in your output, improved where needed and unchanged where
already strong."#;

pub fn extract_placeholders(template: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut i = 0;
    let bytes = template.as_bytes();

    while i + 3 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            if let Some(end_offset) = template[start..].find("}}") {
                let name = &template[start..start + end_offset];
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    let name_str = name.to_string();
                    if !result.contains(&name_str) {
                        result.push(name_str);
                    }
                }
                i = start + end_offset + 2;
            } else {
                i += 2;
            }
        } else {
            i += 1;
        }
    }

    result
}

#[allow(clippy::manual_is_multiple_of)]
pub fn select_template(
    workflow: &Workflow,
    round: u32,
    include_impl_override: Option<bool>,
) -> (&str, bool) {
    let include_impl = match include_impl_override {
        Some(v) => v,
        None => workflow
            .impl_every_n
            .is_some_and(|n| n > 0 && round % n == 0),
    };

    let template = if include_impl {
        workflow
            .template_with_impl
            .as_deref()
            .or(workflow.template.as_deref())
            .unwrap_or(DEFAULT_TEMPLATE_WITH_IMPL)
    } else {
        workflow.template.as_deref().unwrap_or(DEFAULT_TEMPLATE)
    };

    (template, include_impl)
}

pub fn default_documents_map(has_impl: bool) -> HashMap<String, String> {
    let mut map = HashMap::new();
    map.insert("readme".to_string(), "readme".to_string());
    map.insert("spec".to_string(), "spec".to_string());
    if has_impl {
        map.insert("implementation".to_string(), "impl".to_string());
    }
    map
}

pub fn detect_unreferenced_roles(
    documents: &HashMap<String, String>,
    referenced: &[String],
) -> Vec<String> {
    documents
        .keys()
        .filter(|k| !referenced.contains(k))
        .map(|k| {
            format!(
                "Document role '{}' is configured but not referenced by the selected template",
                k
            )
        })
        .collect()
}

#[derive(Debug)]
pub enum RenderError {
    MissingRole(String),
    MissingDocument(String, String),
    KvError(String),
}

pub async fn render_template(
    template: &str,
    workflow: &str,
    documents: &HashMap<String, String>,
    kv: &KvStore,
) -> std::result::Result<(String, Vec<String>), RenderError> {
    let placeholders = extract_placeholders(template);

    let mut contents: HashMap<String, String> = HashMap::new();
    for placeholder in &placeholders {
        let role = documents
            .get(placeholder.as_str())
            .ok_or_else(|| RenderError::MissingRole(placeholder.clone()))?;

        let key = doc_key(workflow, role);
        let text = kv_get_text(kv, &key)
            .await
            .map_err(|e| RenderError::KvError(e.to_string()))?
            .ok_or_else(|| RenderError::MissingDocument(placeholder.clone(), role.clone()))?;

        contents.insert(placeholder.clone(), text);
    }

    let rendered = render_single_pass(template, &contents);

    let warnings = detect_unreferenced_roles(documents, &placeholders);

    Ok((rendered, warnings))
}

fn render_single_pass(template: &str, contents: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut copy_from = 0;
    let mut i = 0;

    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            if let Some(end_offset) = template[start..].find("}}") {
                let name = &template[start..start + end_offset];
                if let Some(content) = contents.get(name) {
                    result.push_str(&template[copy_from..i]);
                    result.push_str(content);
                    i = start + end_offset + 2;
                    copy_from = i;
                    continue;
                }
            }
        }
        i += 1;
    }

    result.push_str(&template[copy_from..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_two_placeholders() {
        let placeholders = extract_placeholders("Hello {{readme}} and {{spec}}");
        assert_eq!(placeholders, vec!["readme", "spec"]);
    }

    #[test]
    fn test_extract_empty_template() {
        let placeholders = extract_placeholders("");
        assert!(placeholders.is_empty());
    }

    #[test]
    fn test_extract_no_placeholders() {
        let placeholders = extract_placeholders("No placeholders here");
        assert!(placeholders.is_empty());
    }

    #[test]
    fn test_extract_duplicate_dedup() {
        let placeholders = extract_placeholders("{{spec}} and {{spec}} again");
        assert_eq!(placeholders, vec!["spec"]);
    }

    #[test]
    fn test_extract_unclosed_braces() {
        let placeholders = extract_placeholders("{{broken");
        assert!(placeholders.is_empty());
    }

    #[test]
    fn test_extract_invalid_chars() {
        let placeholders = extract_placeholders("{{foo bar}}");
        assert!(placeholders.is_empty());
    }

    #[test]
    fn test_extract_hyphen_underscore() {
        let placeholders = extract_placeholders("{{my-impl}} and {{my_spec}}");
        assert_eq!(placeholders, vec!["my-impl", "my_spec"]);
    }

    #[test]
    fn test_extract_from_default_template() {
        let placeholders = extract_placeholders(DEFAULT_TEMPLATE);
        assert_eq!(placeholders, vec!["readme", "spec", "previous_round"]);
    }

    #[test]
    fn test_extract_from_default_impl_template() {
        let placeholders = extract_placeholders(DEFAULT_TEMPLATE_WITH_IMPL);
        assert_eq!(
            placeholders,
            vec!["readme", "implementation", "spec", "previous_round"]
        );
    }

    #[test]
    fn test_select_template_no_impl() {
        let wf = Workflow {
            name: "test".into(),
            description: None,
            provider: "openai".into(),
            model: "o3".into(),
            system_prompt: None,
            provider_params: None,
            documents: HashMap::new(),
            template: Some("custom {{spec}}".into()),
            template_with_impl: None,
            impl_every_n: None,
        };
        let (t, incl) = select_template(&wf, 3, None);
        assert_eq!(t, "custom {{spec}}");
        assert!(!incl);
    }

    #[test]
    fn test_select_template_impl_every_4_on_round_4() {
        let wf = Workflow {
            name: "test".into(),
            description: None,
            provider: "openai".into(),
            model: "o3".into(),
            system_prompt: None,
            provider_params: None,
            documents: HashMap::new(),
            template: Some("base".into()),
            template_with_impl: Some("with-impl".into()),
            impl_every_n: Some(4),
        };
        let (t, incl) = select_template(&wf, 4, None);
        assert_eq!(t, "with-impl");
        assert!(incl);

        let (t, incl) = select_template(&wf, 3, None);
        assert_eq!(t, "base");
        assert!(!incl);
    }

    #[test]
    fn test_select_template_override() {
        let wf = Workflow {
            name: "test".into(),
            description: None,
            provider: "openai".into(),
            model: "o3".into(),
            system_prompt: None,
            provider_params: None,
            documents: HashMap::new(),
            template: Some("base".into()),
            template_with_impl: Some("with-impl".into()),
            impl_every_n: Some(4),
        };
        let (t, incl) = select_template(&wf, 3, Some(true));
        assert_eq!(t, "with-impl");
        assert!(incl);
    }

    #[test]
    fn test_select_template_default_when_none() {
        let wf = Workflow {
            name: "test".into(),
            description: None,
            provider: "openai".into(),
            model: "o3".into(),
            system_prompt: None,
            provider_params: None,
            documents: HashMap::new(),
            template: None,
            template_with_impl: None,
            impl_every_n: None,
        };
        let (t, _) = select_template(&wf, 1, None);
        assert!(t.contains("{{readme}}"));
        assert!(t.contains("{{spec}}"));
    }

    #[test]
    fn test_default_documents_map_no_impl() {
        let map = default_documents_map(false);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("readme").unwrap(), "readme");
        assert_eq!(map.get("spec").unwrap(), "spec");
    }

    #[test]
    fn test_default_documents_map_with_impl() {
        let map = default_documents_map(true);
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("implementation").unwrap(), "impl");
    }

    #[test]
    fn test_detect_unreferenced_roles() {
        let mut docs = HashMap::new();
        docs.insert("readme".into(), "readme".into());
        docs.insert("spec".into(), "spec".into());
        docs.insert("impl".into(), "impl".into());

        let referenced = vec!["readme".to_string(), "spec".to_string()];
        let warnings = detect_unreferenced_roles(&docs, &referenced);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("impl"));
    }

    #[test]
    fn test_detect_no_unreferenced() {
        let mut docs = HashMap::new();
        docs.insert("readme".into(), "readme".into());
        docs.insert("spec".into(), "spec".into());

        let referenced = vec!["readme".to_string(), "spec".to_string()];
        let warnings = detect_unreferenced_roles(&docs, &referenced);
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_render_single_pass_no_double_expansion() {
        let mut contents = HashMap::new();
        contents.insert("readme".to_string(), "See {{spec}} reference".to_string());
        contents.insert("spec".to_string(), "the actual spec".to_string());

        let template = "README: {{readme}}\nSPEC: {{spec}}";
        let rendered = render_single_pass(template, &contents);

        assert!(
            rendered.contains("See {{spec}} reference"),
            "Document content with placeholder-like text must NOT be expanded. Got: {rendered}"
        );
        assert!(rendered.contains("SPEC: the actual spec"));
    }

    #[test]
    fn test_render_single_pass_basic() {
        let mut contents = HashMap::new();
        contents.insert("name".to_string(), "World".to_string());

        let rendered = render_single_pass("Hello {{name}}!", &contents);
        assert_eq!(rendered, "Hello World!");
    }

    #[test]
    fn test_render_single_pass_multiple_same_placeholder() {
        let mut contents = HashMap::new();
        contents.insert("x".to_string(), "val".to_string());

        let rendered = render_single_pass("{{x}} and {{x}}", &contents);
        assert_eq!(rendered, "val and val");
    }

    #[test]
    fn test_render_single_pass_unknown_placeholder_preserved() {
        let contents = HashMap::new();
        let rendered = render_single_pass("Hello {{unknown}}", &contents);
        assert_eq!(rendered, "Hello {{unknown}}");
    }

    #[test]
    fn test_render_single_pass_utf8() {
        let mut contents = HashMap::new();
        contents.insert("greeting".to_string(), "héllo wörld".to_string());

        let rendered = render_single_pass("Say: {{greeting}}!", &contents);
        assert_eq!(rendered, "Say: héllo wörld!");
    }
}
