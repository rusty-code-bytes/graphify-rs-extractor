//! Syntax inventory with the exact vendored PHP grammar used by the extractor.
//! Does not execute PHP or change input files. SQL/HTML are validated separately.
use anyhow::{Context, Result};
use serde_json::json;
use std::{env, fs, path::PathBuf};
fn main() -> Result<()> {
    let args: Vec<_> = env::args().collect();
    let root = PathBuf::from(args.get(1).context("root argument")?);
    let manifest = PathBuf::from(args.get(2).context("manifest argument")?);
    let out = PathBuf::from(args.get(3).context("output argument")?);
    let names: Vec<String> = serde_json::from_slice(&fs::read(manifest)?)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_php::LANGUAGE_PHP.into())?;
    let mut failures = Vec::new();
    let mut depth_candidates = Vec::new();
    let mut checked = 0;
    for name in &names {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if ext == "sql" {
            continue;
        }
        let bytes = fs::read(root.join(name))?;
        checked += 1;
        if bytes.len() > 16 * 1024 * 1024 {
            failures.push(json!({"file":name,"kind":"size_limit","bytes":bytes.len()}));
            continue;
        }
        if std::str::from_utf8(&bytes).is_err() {
            failures.push(json!({"file":name,"kind":"non_utf8"}));
            continue;
        }
        if ext == "html" || ext == "htm" {
            continue;
        }
        let tree = parser.parse(&bytes, None).context("parser cancelled")?;
        let mut stack = vec![(tree.root_node(), 0usize)];
        let mut max_named_depth = 0;
        while let Some((node, depth)) = stack.pop() {
            max_named_depth = max_named_depth.max(depth);
            let named: Vec<_> = node.named_children(&mut node.walk()).collect();
            stack.extend(named.into_iter().map(|child| (child, depth + 1)));
        }
        if max_named_depth > 250 {
            depth_candidates.push(json!({"file":name,"kind":"named_ast_depth_candidate","max_named_depth":max_named_depth}));
        }
        if tree.root_node().has_error() {
            let mut stack = vec![tree.root_node()];
            let mut errors = Vec::new();
            while let Some(node) = stack.pop() {
                if node.is_error() || node.is_missing() {
                    errors.push(json!({"line":node.start_position().row+1,"kind":node.kind(),"missing":node.is_missing(),"text":String::from_utf8_lossy(&bytes[node.start_byte()..node.end_byte().min(node.start_byte()+160)])}));
                    if errors.len() == 10 {
                        break;
                    }
                } else if node.has_error() {
                    let children: Vec<_> = node.children(&mut node.walk()).collect();
                    stack.extend(children.into_iter().rev());
                }
            }
            failures.push(json!({"file":name,"kind":"php_grammar","errors":errors}));
        }
    }
    fs::write(
        out,
        serde_json::to_vec_pretty(
            &json!({"checked":checked,"manifest_files":names.len(),"failures":failures,"depth_candidates":depth_candidates}),
        )?,
    )?;
    println!("checked={checked} failed={}", failures.len());
    Ok(())
}
