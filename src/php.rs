use crate::{
    Stats, html,
    model::{Node, id},
    store::Store,
};
use anyhow::{Context, Result, bail};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};
use tree_sitter::{Node as Ast, Parser};

const MAX_PATH_BYTES: usize = 4096;

/// PHP identifiers are byte strings. Preserve UTF-8 verbatim while giving every
/// invalid byte a reversible, collision-free spelling accepted by tree-sitter.
/// U+F100 is reserved: a literal occurrence is doubled, and an invalid byte is
/// U+F100 followed by U+E000 + byte. Composer keys use this same canonical form.
pub(crate) fn encode_source(source: &[u8]) -> Cow<'_, str> {
    const MARKER: char = '\u{f100}';
    if let Ok(text) = std::str::from_utf8(source) {
        return if text.contains(MARKER) {
            Cow::Owned(text.replace(MARKER, "\u{f100}\u{f100}"))
        } else {
            Cow::Borrowed(text)
        };
    }
    let mut encoded = String::with_capacity(source.len());
    let mut remaining = source;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                encoded.push_str(&text.replace(MARKER, "\u{f100}\u{f100}"));
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                encoded.push_str(
                    &std::str::from_utf8(&remaining[..valid])
                        .unwrap()
                        .replace(MARKER, "\u{f100}\u{f100}"),
                );
                let invalid = error.error_len().unwrap_or(remaining.len() - valid);
                for &byte in &remaining[valid..valid + invalid] {
                    encoded.push(MARKER);
                    encoded.push(char::from_u32(0xe000 + u32::from(byte)).unwrap());
                }
                remaining = &remaining[valid + invalid..];
            }
        }
    }
    Cow::Owned(encoded)
}

/// Reverse `encode_source` for non-identifier data, such as source string path
/// literals and literal HTML. Keep bytes intact; callers requiring filesystem
/// strings must reject an invalid UTF-8 result rather than use lossy decoding.
/// Never apply this to __DIR__/__FILE__, which already contain original paths.
pub(crate) fn decode_source(encoded: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{f100}' {
            if chars.peek() == Some(&'\u{f100}') {
                chars.next();
            } else if let Some(&byte) = chars
                .peek()
                .filter(|&&c| ('\u{e000}'..='\u{e0ff}').contains(&c))
            {
                chars.next();
                bytes.push((byte as u32 - 0xe000) as u8);
                continue;
            }
        }
        let mut utf8 = [0; 4];
        bytes.extend_from_slice(character.encode_utf8(&mut utf8).as_bytes());
    }
    bytes
}

#[derive(Clone, Default)]
struct Scope {
    namespace: String,
    classes: HashMap<String, String>,
    functions: HashMap<String, String>,
    class: Option<String>,
    class_id: Option<String>,
    parent_class: Option<String>,
    has_this: bool,
    receivers: HashMap<String, String>,
    properties: HashMap<String, String>,
    global_hints: HashMap<String, String>,
    owner: String,
    // Values are local to one straight-line execution scope. Object properties
    // require a fresh builtin stdClass; any escape or opaque effect forgets them.
    paths: HashMap<String, (String, bool)>,
    path_objects: HashSet<String>,
    path_clean: bool,
    path_inputs: HashSet<String>,
    activation_eligible: bool,
    activation_reachable: bool,
    deferred: bool,
}

struct Extractor<'a> {
    source: &'a [u8],
    file: &'a str,
    store: &'a Store,
    stats: &'a mut Stats,
    computed_paths_allowed: bool,
}

fn children(node: Ast<'_>) -> Vec<Ast<'_>> {
    node.named_children(&mut node.walk()).collect()
}
fn named(node: Ast<'_>) -> bool {
    matches!(node.kind(), "name" | "qualified_name" | "relative_name")
}
fn qualify(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        name.into()
    } else {
        format!("{namespace}\\{name}")
    }
}

fn unconditional_function(node: Ast<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "program"
            || parent.kind() == "compound_statement"
                && parent
                    .parent()
                    .is_some_and(|namespace| namespace.kind() == "namespace_definition")
    })
}

fn has_jump(nodes: &[Ast<'_>]) -> bool {
    let mut pending = nodes.to_vec();
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "goto_statement" | "named_label_statement") {
            return true;
        }
        if !matches!(
            node.kind(),
            "function_definition"
                | "method_declaration"
                | "anonymous_function"
                | "arrow_function"
                | "class_declaration"
                | "anonymous_class"
                | "interface_declaration"
                | "trait_declaration"
                | "enum_declaration"
        ) {
            pending.extend(children(node));
        }
    }
    false
}

fn has_ticks(root: Ast<'_>, source: &[u8]) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "declare_directive"
            && source[node.byte_range()]
                .windows(5)
                .any(|word| word.eq_ignore_ascii_case(b"ticks"))
        {
            return true;
        }
        pending.extend(children(node));
    }
    false
}

impl Extractor<'_> {
    fn loader_unknown(&self, scope: &Scope) -> Result<()> {
        self.store.symbol(
            if scope.deferred {
                "php-loader-unknown-deferred"
            } else {
                "php-loader-unknown-init"
            },
            self.file,
            &scope.owner,
        )
    }
    fn direct_event(&self, node: Ast<'_>, scope: &Scope) -> usize {
        if scope.activation_eligible
            && scope.activation_reachable
            && node.parent().is_some_and(|parent| {
                parent.kind() == "expression_statement" && parent.named_child(0) == Some(node)
            })
        {
            node.start_byte() + 1
        } else {
            0
        }
    }
    fn text(&self, node: Ast<'_>) -> &str {
        std::str::from_utf8(&self.source[node.byte_range()]).unwrap()
    }
    fn resolve_name(&self, raw: &str, scope: &Scope, function: bool) -> String {
        if let Some(absolute) = raw.strip_prefix('\\') {
            return absolute.to_ascii_lowercase();
        }
        if raw.to_ascii_lowercase().starts_with("namespace\\") {
            return qualify(&scope.namespace, &raw[10..]).to_ascii_lowercase();
        }
        let (first, rest) = raw.split_once('\\').unwrap_or((raw, ""));
        let aliases = if function && rest.is_empty() {
            &scope.functions
        } else {
            &scope.classes
        };
        if let Some(target) = aliases.get(&first.to_ascii_lowercase()) {
            return if rest.is_empty() {
                target.clone()
            } else {
                format!("{target}\\{rest}").to_ascii_lowercase()
            };
        }
        qualify(&scope.namespace, raw).to_ascii_lowercase()
    }
    fn type_class(&self, raw: &str, scope: &Scope) -> Option<String> {
        match raw.to_ascii_lowercase().as_str() {
            "self" => scope.class_id.as_ref().map(|id| format!("@{id}")),
            "parent" => scope.parent_class.clone(),
            "static" | "array" | "bool" | "callable" | "false" | "float" | "int" | "iterable"
            | "mixed" | "never" | "null" | "object" | "string" | "true" | "void" => None,
            _ => Some(self.resolve_name(raw, scope, false)),
        }
    }
    fn single_type_class(&self, mut ty: Ast<'_>, scope: &Scope) -> Option<String> {
        if ty.kind() == "optional_type" {
            ty = ty.named_child(0)?;
        }
        // A union/intersection supplies useful references, but cannot identify
        // one receiver implementation. Late static binding is dynamic too.
        (ty.kind() == "named_type")
            .then(|| self.type_class(self.text(ty), scope))
            .flatten()
    }
    fn type_references(&self, ty: Ast<'_>, scope: &Scope, context: &str) -> Result<()> {
        let mut pending = vec![ty];
        while let Some(current) = pending.pop() {
            if current.kind() == "named_type" {
                if let Some(target) = self.type_class(self.text(current), scope) {
                    self.store.reference(
                        &scope.owner,
                        "php-class",
                        &target,
                        None,
                        context,
                        self.file,
                        current.start_position().row + 1,
                    )?;
                }
            } else {
                pending.extend(children(current).into_iter().rev());
            }
        }
        Ok(())
    }
    fn class_properties(&self, node: Ast<'_>, scope: &Scope) -> HashMap<String, String> {
        let mut declarations = HashMap::<String, Option<String>>::new();
        let Some(body) = node.child_by_field_name("body") else {
            return HashMap::new();
        };
        let mut add = |name: Ast<'_>, ty: Option<Ast<'_>>| {
            let class = ty.and_then(|ty| self.single_type_class(ty, scope));
            declarations
                .entry(self.text(name).trim_start_matches('$').to_owned())
                .and_modify(|target| *target = None)
                .or_insert(class);
        };
        for declaration in children(body) {
            if declaration.kind() == "property_declaration" {
                let fields = children(declaration);
                let usable = !fields
                    .iter()
                    .any(|child| matches!(child.kind(), "static_modifier" | "property_hook_list"));
                let ty = usable
                    .then(|| declaration.child_by_field_name("type"))
                    .flatten();
                for element in fields
                    .iter()
                    .filter(|child| child.kind() == "property_element")
                {
                    if let Some(name) = element.child_by_field_name("name") {
                        add(name, ty);
                    }
                }
            } else if declaration.kind() == "method_declaration"
                && declaration
                    .child_by_field_name("name")
                    .is_some_and(|name| self.text(name).eq_ignore_ascii_case("__construct"))
                && let Some(parameters) = declaration.child_by_field_name("parameters")
            {
                for parameter in children(parameters)
                    .into_iter()
                    .filter(|parameter| parameter.kind() == "property_promotion_parameter")
                {
                    if let Some(name) = parameter
                        .child_by_field_name("name")
                        .filter(|name| name.kind() == "variable_name")
                    {
                        let ty = (!children(parameter)
                            .iter()
                            .any(|child| child.kind() == "property_hook_list"))
                        .then(|| parameter.child_by_field_name("type"))
                        .flatten();
                        add(name, ty);
                    }
                }
            }
        }
        let mut class_scope = scope.clone();
        class_scope.has_this = true;
        class_scope.properties = declarations
            .into_iter()
            .filter_map(|(name, class)| class.map(|class| (name, class)))
            .collect();
        // Constructor writes and escapes in other methods can replace the
        // receiver before a seemingly read-only caller runs. Promoted fields
        // retain their declared-type hint without inventing a constructor value.
        self.property_receivers(node, &class_scope)
    }
    fn property_receivers(&self, node: Ast<'_>, scope: &Scope) -> HashMap<String, String> {
        if !scope.has_this || scope.properties.is_empty() {
            return HashMap::new();
        }
        let mut properties = scope.properties.clone();
        let mut pending: Vec<_> = node.child_by_field_name("body").into_iter().collect();
        while let Some(current) = pending.pop() {
            if matches!(
                current.kind(),
                "class_declaration" | "anonymous_class" | "trait_declaration"
            ) {
                continue;
            }
            if matches!(
                current.kind(),
                "dynamic_variable_name"
                    | "eval_intrinsic"
                    | "include_expression"
                    | "include_once_expression"
                    | "require_expression"
                    | "require_once_expression"
            ) {
                return HashMap::new();
            }
            if current.kind() == "function_call_expression"
                && current
                    .child_by_field_name("function")
                    .is_some_and(|callee| self.text(callee).eq_ignore_ascii_case("eval"))
            {
                return HashMap::new();
            }
            if current.kind() == "variable_name" && self.text(current) == "$this" {
                let Some(access) = current.parent().filter(|access| {
                    matches!(
                        access.kind(),
                        "member_access_expression" | "nullsafe_member_access_expression"
                    ) && access.child_by_field_name("object") == Some(current)
                }) else {
                    // Passing $this or invoking an opaque method can replace a
                    // property with a subtype overriding the declared method.
                    return HashMap::new();
                };
                let Some(name) = access
                    .child_by_field_name("name")
                    .filter(|name| name.kind() == "name")
                else {
                    return HashMap::new();
                };
                let receiver_only = access.parent().is_some_and(|call| {
                    matches!(
                        call.kind(),
                        "member_call_expression" | "nullsafe_member_call_expression"
                    ) && call.child_by_field_name("object") == Some(access)
                });
                if !receiver_only {
                    properties.remove(self.text(name));
                }
            }
            pending.extend(children(current));
        }
        properties
    }
    fn parameter_receivers(&self, node: Ast<'_>, scope: &Scope) -> HashMap<String, String> {
        let mut receivers = HashMap::new();
        if let Some(parameters) = node.child_by_field_name("parameters") {
            for parameter in children(parameters) {
                if parameter.kind() == "variadic_parameter"
                    || parameter
                        .child_by_field_name("reference_modifier")
                        .is_some()
                {
                    continue;
                }
                let (Some(name), Some(ty)) = (
                    parameter.child_by_field_name("name"),
                    parameter.child_by_field_name("type"),
                ) else {
                    continue;
                };
                if name.kind() != "variable_name" {
                    continue;
                }
                if let Some(class) = self.single_type_class(ty, scope) {
                    receivers.insert(self.text(name).to_owned(), class);
                }
            }
        }
        if receivers.is_empty() {
            return receivers;
        }
        let Some(body) = node.child_by_field_name("body") else {
            return receivers;
        };
        // Only infer for parameters used exclusively as receivers. Passing a parameter
        // to an unknown by-reference callee or assigning it invalidates the entire hint.
        let mut invalid = HashSet::new();
        let mut cursor = body.walk();
        loop {
            let current = cursor.node();
            if matches!(
                current.kind(),
                "dynamic_variable_name"
                    | "eval_intrinsic"
                    | "include_expression"
                    | "include_once_expression"
                    | "require_expression"
                    | "require_once_expression"
            ) {
                return HashMap::new();
            }
            if current.kind() == "function_call_expression"
                && let Some(callee) = current.child_by_field_name("function")
            {
                let target = self.resolve_name(self.text(callee), scope, true);
                if self.mutates_symbol_table(current, &target) {
                    return HashMap::new();
                }
            }
            if current.kind() == "variable_name" && receivers.contains_key(self.text(current)) {
                let receiver_use = current.parent().is_some_and(|parent| {
                    matches!(
                        parent.kind(),
                        "member_call_expression" | "nullsafe_member_call_expression"
                    ) && parent.child_by_field_name("object") == Some(current)
                });
                if !receiver_use {
                    invalid.insert(self.text(current).to_owned());
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    receivers.retain(|name, _| !invalid.contains(name));
                    return receivers;
                }
            }
        }
    }
    fn constructor_class(&self, node: Ast<'_>, scope: &Scope) -> Option<String> {
        let class = children(node).into_iter().find(|n| named(*n))?;
        let raw = self.text(class);
        match raw.to_ascii_lowercase().as_str() {
            "self" => scope.class_id.as_ref().map(|id| format!("@{id}")),
            "static" | "parent" => None,
            _ => Some(self.resolve_name(raw, scope, false)),
        }
    }
    fn mutates_symbol_table(&self, call: Ast<'_>, target: &str) -> bool {
        match target.rsplit('\\').next() {
            Some("extract" | "eval") => true,
            // With an explicit output array parse_str only writes that argument.
            Some("parse_str") => call
                .child_by_field_name("arguments")
                .is_none_or(|args| args.named_child_count() < 2),
            _ => false,
        }
    }
    fn effects(&self, node: Ast<'_>, scope: &Scope) -> (HashSet<String>, bool) {
        let mut invalid = HashSet::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if matches!(
                n.kind(),
                "function_definition"
                    | "method_declaration"
                    | "anonymous_function"
                    | "arrow_function"
                    | "class_declaration"
                    | "anonymous_class"
                    | "trait_declaration"
                    | "interface_declaration"
            ) {
                continue;
            }
            if matches!(
                n.kind(),
                "dynamic_variable_name"
                    | "eval_intrinsic"
                    | "include_expression"
                    | "include_once_expression"
                    | "require_expression"
                    | "require_once_expression"
            ) {
                return (invalid, true);
            }
            if n.kind() == "function_call_expression"
                && let Some(callee) = n.child_by_field_name("function")
                && self.mutates_symbol_table(n, &self.resolve_name(self.text(callee), scope, true))
            {
                return (invalid, true);
            }
            if matches!(
                n.kind(),
                "assignment_expression"
                    | "augmented_assignment_expression"
                    | "reference_assignment_expression"
            ) {
                if let Some(left) = n.child_by_field_name("left")
                    && left.kind() == "variable_name"
                {
                    invalid.insert(self.text(left).to_owned());
                }
                if n.kind() == "reference_assignment_expression"
                    && let Some(right) = n.child_by_field_name("right")
                    && right.kind() == "variable_name"
                {
                    invalid.insert(self.text(right).to_owned());
                }
            }
            if matches!(
                n.kind(),
                "global_declaration" | "catch_clause" | "unset_statement"
            ) {
                for var in children(n)
                    .into_iter()
                    .filter(|c| c.kind() == "variable_name")
                {
                    invalid.insert(self.text(var).to_owned());
                }
            }
            if n.kind() == "foreach_statement" {
                // The first child is the collection; subsequent header children bind iteration variables.
                for header in children(n)
                    .into_iter()
                    .skip(1)
                    .filter(|c| Some(*c) != n.child_by_field_name("body"))
                {
                    let mut pending = vec![header];
                    while let Some(v) = pending.pop() {
                        if v.kind() == "variable_name" {
                            invalid.insert(self.text(v).to_owned());
                        } else {
                            pending.extend(children(v));
                        }
                    }
                }
            }
            if n.kind() == "variable_name"
                && n.parent()
                    .is_some_and(|p| matches!(p.kind(), "argument" | "update_expression"))
            {
                invalid.insert(self.text(n).to_owned());
            }
            stack.extend(children(n));
        }
        (invalid, false)
    }
    fn sequence(&mut self, nodes: &[Ast<'_>], mut scope: Scope, depth: usize) -> Result<()> {
        for &node in nodes {
            self.walk(node, &scope, depth + 1)?;
            self.path_effects(node, &mut scope);
            if matches!(
                node.kind(),
                "return_statement" | "throw_expression" | "exit_statement"
            ) || node.kind() == "expression_statement"
                && node
                    .named_child(0)
                    .is_some_and(|expression| expression.kind() == "throw_expression")
            {
                scope.activation_reachable = false;
            }
            let (invalid, clear) = self.effects(node, &scope);
            if clear {
                scope.receivers.clear();
            } else {
                scope.receivers.retain(|name, _| !invalid.contains(name));
            }
            if node.kind() == "global_declaration" {
                for var in children(node) {
                    if let Some(class) = scope.global_hints.get(self.text(var)) {
                        scope
                            .receivers
                            .insert(self.text(var).to_owned(), class.clone());
                    }
                }
            }
            let expr = if node.kind() == "expression_statement" {
                node.named_child(0)
            } else {
                Some(node)
            };
            if let Some(expr) = expr.filter(|e| e.kind() == "assignment_expression")
                && let (Some(left), Some(right)) = (
                    expr.child_by_field_name("left"),
                    expr.child_by_field_name("right"),
                )
                && left.kind() == "variable_name"
                && right.kind() == "object_creation_expression"
                && let Some(class) = self.constructor_class(right, &scope)
            {
                scope.receivers.insert(self.text(left).to_owned(), class);
            }
        }
        Ok(())
    }
    fn global_receivers(&self, node: Ast<'_>, scope: &Scope) -> HashMap<String, String> {
        let mut result = HashMap::new();
        let mut stack: Vec<_> = node.child_by_field_name("body").into_iter().collect();
        while let Some(n) = stack.pop() {
            if matches!(
                n.kind(),
                "function_definition"
                    | "anonymous_function"
                    | "arrow_function"
                    | "class_declaration"
            ) {
                continue;
            }
            if n.kind() == "global_declaration" {
                for var in children(n) {
                    if let Some(class) = scope.receivers.get(self.text(var)) {
                        result.insert(self.text(var).to_owned(), class.clone());
                    }
                }
            }
            stack.extend(children(n));
        }
        result
    }
    fn path_key(&self, node: Ast<'_>, scope: &Scope) -> Option<String> {
        if node.kind() == "variable_name" {
            return Some(self.text(node).to_owned());
        }
        if node.kind() == "member_access_expression"
            && let (Some(object), Some(name)) = (
                node.child_by_field_name("object"),
                node.child_by_field_name("name"),
            )
            && object.kind() == "variable_name"
            && name.kind() == "name"
            && scope.path_objects.contains(self.text(object))
        {
            return Some(format!("{}->{}", self.text(object), self.text(name)));
        }
        None
    }
    fn path_effects(&self, node: Ast<'_>, scope: &mut Scope) {
        if !scope.activation_eligible || !scope.path_clean {
            scope.paths.clear();
            scope.path_objects.clear();
            return;
        }
        // Definitions are dormant. All other unmodelled statements may mutate
        // globals, aliases or references, including calls and every include.
        if matches!(
            node.kind(),
            "php_tag" | "comment" | "namespace_use_declaration" | "function_definition"
        ) {
            return;
        }
        let assignment = (node.kind() == "expression_statement")
            .then(|| node.named_child(0))
            .flatten()
            .filter(|expr| expr.kind() == "assignment_expression");
        if let Some(expr) = assignment
            && let (Some(left), Some(right)) = (
                expr.child_by_field_name("left"),
                expr.child_by_field_name("right"),
            )
        {
            // Alias/reference assignments cannot retain the original object's proof.
            if let Some(key) = self.path_key(left, scope)
                && !scope.path_inputs.contains(&key)
                && let Some(value) = self.include_value(right, scope, 0)
            {
                if left.kind() == "variable_name" && scope.path_objects.contains(&key) {
                    scope.paths.clear();
                    scope.path_objects.clear();
                }
                if scope.paths.len() >= 256 {
                    scope.paths.clear();
                    scope.path_objects.clear();
                }
                scope.paths.insert(key, value);
                return;
            }
            if left.kind() == "variable_name"
                && right.kind() == "object_creation_expression"
                && !scope.path_inputs.contains(self.text(left))
                && self.constructor_class(right, scope).as_deref() == Some("stdclass")
                && children(right)
                    .iter()
                    .all(|child| child.kind() != "arguments" || child.named_child_count() == 0)
                && children(right).iter().any(|child| {
                    named(*child) && self.text(*child).eq_ignore_ascii_case("\\stdClass")
                })
            {
                // Replacing an object may leave aliases; forget all old facts first.
                scope.paths.clear();
                scope.path_objects.clear();
                scope.path_objects.insert(self.text(left).to_owned());
                return;
            }
        }
        scope.paths.clear();
        scope.path_objects.clear();
        scope.path_clean = false;
    }
    fn include_value(&self, node: Ast<'_>, scope: &Scope, depth: usize) -> Option<(String, bool)> {
        if depth > 32 {
            return None;
        }
        match node.kind() {
            "name" if self.text(node).eq_ignore_ascii_case("__DIR__") => Some((
                self.file
                    .rsplit_once('/')
                    .map(|(dir, _)| dir)
                    .unwrap_or("")
                    .into(),
                true,
            )),
            "name" if self.text(node).eq_ignore_ascii_case("__FILE__") => {
                Some((self.file.into(), true))
            }
            "variable_name" | "member_access_expression"
                if scope.activation_eligible && scope.path_clean =>
            {
                scope.paths.get(&self.path_key(node, scope)?).cloned()
            }
            "parenthesized_expression" => {
                self.include_value(node.named_child(0)?, scope, depth + 1)
            }
            "binary_expression" if self.text(node.child_by_field_name("operator")?) == "." => {
                let (left, anchored) =
                    self.include_value(node.child_by_field_name("left")?, scope, depth + 1)?;
                let (right, _) =
                    self.include_value(node.child_by_field_name("right")?, scope, depth + 1)?;
                (left.len().checked_add(right.len())? <= MAX_PATH_BYTES)
                    .then(|| (left + &right, anchored))
            }
            "function_call_expression" => {
                let callee = node.child_by_field_name("function")?;
                let raw = self.text(callee);
                // A namespace or use-function alias may replace unqualified dirname.
                if !(raw.eq_ignore_ascii_case("\\dirname")
                    || raw.eq_ignore_ascii_case("dirname")
                        && scope.namespace.is_empty()
                        && !scope.functions.contains_key("dirname"))
                {
                    return None;
                }
                let args = node.child_by_field_name("arguments")?;
                if args.named_child_count() != 1 {
                    return None;
                }
                let argument = args.named_child(0)?.named_child(0)?;
                if argument.kind() != "name" || !self.text(argument).eq_ignore_ascii_case("__DIR__")
                {
                    return None;
                }
                let (directory, anchored) = self.include_value(argument, scope, depth + 1)?;
                if directory.is_empty() {
                    return None;
                }
                Some((
                    directory
                        .rsplit_once('/')
                        .map(|(parent, _)| parent)
                        .unwrap_or("")
                        .to_owned(),
                    anchored,
                ))
            }
            "string" | "encapsed_string" => {
                let raw = self.text(node);
                let quote = raw.chars().next()?;
                if !matches!(quote, '\'' | '"') || !raw.ends_with(quote) {
                    return None;
                }
                let inner = &raw[1..raw.len() - 1];
                // Conservative constant evaluation: interpolated/escaped paths stay unresolved.
                if inner.len() > MAX_PATH_BYTES
                    || inner.contains('\\')
                    || quote == '"' && inner.contains('$')
                {
                    return None;
                }
                Some((String::from_utf8(decode_source(inner)).ok()?, false))
            }
            _ => None,
        }
    }
    fn include_target(&self, node: Ast<'_>, scope: &Scope) -> Option<String> {
        let (value, anchored) = self.include_value(node, scope, 0)?;
        if !anchored || value.contains(['\0', '\\']) {
            return None;
        }
        let ancestors: Vec<_> = self
            .file
            .rsplit_once('/')
            .map(|(dir, _)| dir.split('/').collect())
            .unwrap_or_default();
        let raw_parts: Vec<_> = value
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect();
        let trusted = raw_parts
            .iter()
            .zip(&ancestors)
            .take_while(|(part, ancestor)| part == ancestor)
            .count();
        let mut parts: Vec<(&str, bool)> = Vec::new();
        for (index, part) in raw_parts.into_iter().enumerate() {
            match part {
                "" | "." => {}
                ".." => {
                    // `link/..` follows the symlink before walking to its parent.
                    // Only source ancestry (the walker never follows symlinks)
                    // can safely be cancelled without filesystem evaluation.
                    let (_, safe) = parts.pop()?;
                    if !safe {
                        return None;
                    }
                }
                part => parts.push((part, index < trusted)),
            }
        }
        Some(
            parts
                .into_iter()
                .map(|(part, _)| part)
                .collect::<Vec<_>>()
                .join("/"),
        )
    }
    fn imports(&self, node: Ast<'_>, scope: &mut Scope) -> Result<()> {
        let declaration_type = node
            .child_by_field_name("type")
            .map(|n| self.text(n))
            .unwrap_or("");
        let group = node.child_by_field_name("body");
        let prefix = if group.is_some() {
            children(node)
                .into_iter()
                .find(|c| c.kind() == "namespace_name")
                .map(|n| self.text(n))
                .unwrap_or("")
        } else {
            ""
        };
        let clauses = children(group.unwrap_or(node));
        for clause in clauses
            .into_iter()
            .filter(|n| n.kind() == "namespace_use_clause")
        {
            let kind = clause
                .child_by_field_name("type")
                .map(|n| self.text(n))
                .unwrap_or(declaration_type);
            if kind == "const" {
                continue;
            }
            let alias_node = clause.child_by_field_name("alias");
            let name_node = children(clause).into_iter().find(|n| {
                Some(*n) != alias_node
                    && matches!(n.kind(), "qualified_name" | "namespace_name" | "name")
            });
            let Some(name_node) = name_node else {
                bail!(
                    "unsupported PHP use clause at line {}",
                    clause.start_position().row + 1
                );
            };
            let raw = self.text(name_node).trim_matches('\\');
            let target = qualify(prefix.trim_matches('\\'), raw).to_ascii_lowercase();
            let alias = alias_node
                .map(|n| self.text(n))
                .unwrap_or_else(|| raw.rsplit('\\').next().unwrap())
                .to_ascii_lowercase();
            let aliases = if kind == "function" {
                &mut scope.functions
            } else {
                &mut scope.classes
            };
            if aliases.insert(alias.clone(), target.clone()).is_some() {
                bail!("duplicate PHP import alias {alias}");
            }
            self.store.reference(
                &scope.owner,
                if kind == "function" {
                    "php-function"
                } else {
                    "php-class"
                },
                &target,
                None,
                "imports",
                self.file,
                clause.start_position().row + 1,
            )?;
        }
        Ok(())
    }
    fn section(&mut self, nodes: &[Ast<'_>], mut scope: Scope, depth: usize) -> Result<()> {
        for &node in nodes {
            if node.kind() == "namespace_use_declaration" {
                self.imports(node, &mut scope)?;
            }
        }
        self.sequence(nodes, scope, depth)
    }
    fn walk(&mut self, node: Ast<'_>, scope: &Scope, depth: usize) -> Result<()> {
        // Only lexical/control-flow scopes consume the bounded Rust call stack.
        // Ordinary expression depth (e.g. thousands of concatenations) is walked
        // iteratively, preserving source order and the current scope.
        if depth > 256 {
            bail!("PHP lexical scopes exceed traversal depth limit");
        }
        let mut pending = vec![node];
        while let Some(current) = pending.pop() {
            if self.walk_one(current, scope, depth)? {
                pending.extend(children(current).into_iter().rev());
            }
        }
        Ok(())
    }
    fn walk_one(&mut self, node: Ast<'_>, scope: &Scope, depth: usize) -> Result<bool> {
        let line = node.start_position().row + 1;
        match node.kind() {
            "compound_statement" => {
                self.sequence(&children(node), scope.clone(), depth)?;
                return Ok(false);
            }
            "if_statement" | "while_statement" | "do_statement" | "for_statement"
            | "foreach_statement" | "switch_statement" | "try_statement" | "catch_clause"
            | "declare_statement" => {
                let mut next = scope.clone();
                next.activation_eligible = false;
                next.paths.clear();
                next.path_objects.clear();
                next.path_clean = false;
                let (invalid, clear) = self.effects(node, scope);
                if clear {
                    next.receivers.clear();
                } else {
                    next.receivers.retain(|name, _| !invalid.contains(name));
                }
                for child in children(node) {
                    self.walk(child, &next, depth + 1)?;
                }
                return Ok(false);
            }
            "namespace_use_declaration" => return Ok(false),
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "anonymous_class"
            | "function_definition"
            | "method_declaration"
            | "anonymous_function"
            | "arrow_function" => {
                let is_class = matches!(
                    node.kind(),
                    "class_declaration"
                        | "interface_declaration"
                        | "trait_declaration"
                        | "enum_declaration"
                        | "anonymous_class"
                );
                let anonymous = matches!(
                    node.kind(),
                    "anonymous_class" | "anonymous_function" | "arrow_function"
                );
                let name = node
                    .child_by_field_name("name")
                    .map(|n| self.text(n).to_owned())
                    .unwrap_or_else(|| format!("anonymous@{}", node.start_byte()));
                let fq = if node.kind() == "method_declaration" {
                    format!(
                        "{}::{name}",
                        scope.class.as_deref().context("method outside class")?
                    )
                } else if anonymous {
                    format!("{}::{name}", scope.owner)
                } else {
                    qualify(&scope.namespace, &name)
                };
                let kind = if is_class {
                    node.kind().trim_end_matches("_declaration")
                } else if node.kind() == "method_declaration" {
                    "method"
                } else {
                    "function"
                };
                let mut entity = Node::new(self.file, kind, &fq, "php", line);
                entity.id = id(&[self.file, kind, &fq, &node.start_byte().to_string()]);
                self.store.node(&entity)?;
                self.store
                    .edge(&scope.owner, &entity.id, "defines", self.file, line)?;
                if !anonymous {
                    self.store.symbol(
                        if is_class {
                            "php-class"
                        } else if kind == "method" {
                            "php-method"
                        } else if !unconditional_function(node) {
                            // A runtime guard, enclosing callable, or control
                            // flow may suppress this declaration. Retain it for
                            // inventory without letting it shadow a definite
                            // unconditional function of the same name.
                            "php-function-conditional"
                        } else {
                            "php-function"
                        },
                        &fq.to_ascii_lowercase(),
                        &entity.id,
                    )?;
                }
                if is_class {
                    self.store
                        .symbol("php-class", &format!("@{}", entity.id), &entity.id)?;
                } else if kind == "method"
                    && let Some(owner) = &scope.class_id
                {
                    self.store.method(owner, &name, &entity.id)?;
                }
                let mut next = scope.clone();
                next.owner = entity.id.clone();
                next.paths.clear();
                next.path_objects.clear();
                next.activation_eligible = !is_class
                    && !node
                        .child_by_field_name("body")
                        .is_some_and(|body| has_jump(&children(body)));
                next.activation_reachable = true;
                next.deferred |= !is_class;
                // File variables and closure captures may already hold objects
                // whose destructors run on assignment. Fresh named callable
                // locals have a bounded clean symbol table until an opaque
                // statement; parameters always remain unknown inputs.
                next.path_clean = self.computed_paths_allowed
                    && matches!(node.kind(), "function_definition" | "method_declaration");
                next.path_inputs = node
                    .child_by_field_name("parameters")
                    .map(children)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|parameter| parameter.child_by_field_name("name"))
                    .map(|name| self.text(name).to_owned())
                    .collect();
                next.path_inputs.extend(
                    [
                        "$GLOBALS",
                        "$_SESSION",
                        "$_GET",
                        "$_POST",
                        "$_SERVER",
                        "$_ENV",
                        "$_FILES",
                        "$_COOKIE",
                        "$_REQUEST",
                        "$this",
                    ]
                    .into_iter()
                    .map(str::to_owned),
                );
                if is_class {
                    next.class_id = Some(entity.id);
                    next.class = Some(fq);
                    next.parent_class = children(node)
                        .into_iter()
                        .find(|child| child.kind() == "base_clause")
                        .and_then(|base| children(base).into_iter().find(|child| named(*child)))
                        .map(|parent| self.resolve_name(self.text(parent), scope, false));
                    next.has_this = false;
                    next.receivers.clear();
                    next.properties = self.class_properties(node, &next);
                } else if node.kind() == "function_definition" {
                    next.class = None;
                    next.class_id = None;
                    next.parent_class = None;
                    next.has_this = false;
                    next.properties.clear();
                }
                if !is_class {
                    let is_static = children(node).iter().any(|c| c.kind() == "static_modifier");
                    if node.kind() == "method_declaration" {
                        next.has_this = !is_static;
                    } else if is_static {
                        next.has_this = false;
                    }
                    next.receivers = self.parameter_receivers(node, &next);
                    next.properties = self.property_receivers(node, &next);
                    next.global_hints = if node.kind() == "function_definition"
                        || node.kind() == "method_declaration"
                    {
                        self.global_receivers(node, scope)
                    } else {
                        HashMap::new()
                    };
                    if let Some(ty) = node.child_by_field_name("return_type") {
                        self.type_references(ty, &next, "references:return_type")?;
                    }
                }
                for child in children(node) {
                    self.walk(child, &next, depth + 1)?;
                }
                return Ok(false);
            }
            "simple_parameter" | "variadic_parameter" | "property_promotion_parameter" => {
                if let Some(ty) = node.child_by_field_name("type") {
                    self.type_references(ty, scope, "references:parameter_type")?;
                    if node.kind() == "property_promotion_parameter"
                        && let Some(class_id) = &scope.class_id
                    {
                        let mut class_scope = scope.clone();
                        class_scope.owner = class_id.clone();
                        self.type_references(ty, &class_scope, "references:field")?;
                    }
                }
            }
            "property_declaration" => {
                if let Some(ty) = node.child_by_field_name("type") {
                    self.type_references(ty, scope, "references:field")?;
                }
            }
            "attribute" => {
                if let Some(name) = children(node).into_iter().find(|child| named(*child)) {
                    self.store.reference(
                        &scope.owner,
                        "php-class",
                        &self.resolve_name(self.text(name), scope, false),
                        None,
                        "references:attribute",
                        self.file,
                        line,
                    )?;
                }
            }
            "base_clause" | "class_interface_clause" | "use_declaration" => {
                if node.kind() == "use_declaration"
                    && self.text(node).contains('{')
                    && let Some(owner) = &scope.class_id
                {
                    self.store.opaque_traits(owner)?;
                }
                for child in children(node).into_iter().filter(|n| named(*n)) {
                    self.store.reference(
                        &scope.owner,
                        "php-class",
                        &self.resolve_name(self.text(child), scope, false),
                        None,
                        match node.kind() {
                            "base_clause" => "extends",
                            "class_interface_clause" => "implements",
                            _ => "uses_trait",
                        },
                        self.file,
                        line,
                    )?;
                }
            }
            "function_call_expression" => {
                let callee = node
                    .child_by_field_name("function")
                    .context("missing function call target")?;
                if named(callee) {
                    let raw = self.text(callee);
                    // This grammar represents PHP's eval construct as a named
                    // call. Its evaluated declarations are not ordinary symbols.
                    if raw.eq_ignore_ascii_case("eval") {
                        self.loader_unknown(scope)?;
                        return Ok(true);
                    }
                    let target = self.resolve_name(raw, scope, true);
                    let fallback = (!raw.contains('\\')
                        && !scope.functions.contains_key(&raw.to_ascii_lowercase())
                        && !scope.namespace.is_empty())
                    .then(|| raw.to_ascii_lowercase());
                    self.store.php_function_reference(
                        &scope.owner,
                        &target,
                        fallback.as_deref(),
                        self.file,
                        line,
                        self.direct_event(node, scope),
                    )?;
                } else {
                    self.stats.dynamic_php_references += 1;
                }
            }
            "scoped_call_expression" => {
                let class = node
                    .child_by_field_name("scope")
                    .context("missing static call scope")?;
                let method = node
                    .child_by_field_name("name")
                    .context("missing method name")?;
                let raw = self.text(class);
                if raw.eq_ignore_ascii_case("parent")
                    && method.kind() == "name"
                    && let Some(class_id) = &scope.class_id
                {
                    self.store.reference(
                        &scope.owner,
                        "php-parent-method",
                        &format!("{class_id}::{}", self.text(method).to_ascii_lowercase()),
                        None,
                        "calls",
                        self.file,
                        line,
                    )?;
                    return Ok(true);
                }
                let target_class = if raw.eq_ignore_ascii_case("self") {
                    scope.class_id.as_ref().map(|id| format!("@{id}"))
                } else if named(class)
                    && !matches!(raw.to_ascii_lowercase().as_str(), "static" | "parent")
                {
                    Some(self.resolve_name(raw, scope, false))
                } else {
                    None
                };
                if let Some(class) = target_class.filter(|_| method.kind() == "name") {
                    self.store.reference(
                        &scope.owner,
                        "php-method",
                        &format!("{class}::{}", self.text(method)).to_ascii_lowercase(),
                        None,
                        "calls",
                        self.file,
                        line,
                    )?;
                } else {
                    self.stats.dynamic_php_references += 1;
                }
            }
            "member_call_expression" | "nullsafe_member_call_expression" => {
                let receiver = node
                    .child_by_field_name("object")
                    .context("missing receiver")?;
                let method = node.child_by_field_name("name").context("missing method")?;
                let class = if receiver.kind() == "variable_name" && method.kind() == "name" {
                    let raw = self.text(receiver);
                    if raw == "$this" && scope.has_this {
                        scope.class_id.as_ref().map(|id| format!("@{id}"))
                    } else {
                        scope.receivers.get(raw).cloned()
                    }
                } else if method.kind() == "name"
                    && scope.has_this
                    && matches!(
                        receiver.kind(),
                        "member_access_expression" | "nullsafe_member_access_expression"
                    )
                    && receiver
                        .child_by_field_name("object")
                        .is_some_and(|object| {
                            object.kind() == "variable_name" && self.text(object) == "$this"
                        })
                {
                    receiver
                        .child_by_field_name("name")
                        .filter(|name| name.kind() == "name")
                        .and_then(|name| scope.properties.get(self.text(name)).cloned())
                } else {
                    None
                };
                if let Some(class) = class {
                    self.store.reference(
                        &scope.owner,
                        "php-method-inferred",
                        &format!("{class}::{}", self.text(method)).to_ascii_lowercase(),
                        None,
                        "calls",
                        self.file,
                        line,
                    )?;
                } else {
                    self.stats.dynamic_php_references += 1;
                }
            }
            "object_creation_expression" => {
                if let Some(class) = self.constructor_class(node, scope) {
                    self.store.reference(
                        &scope.owner,
                        "php-class",
                        &class,
                        None,
                        "instantiates",
                        self.file,
                        line,
                    )?;
                } else {
                    self.stats.dynamic_php_references += 1;
                }
            }
            "include_expression"
            | "include_once_expression"
            | "require_expression"
            | "require_once_expression" => {
                let target = node
                    .named_child(0)
                    .and_then(|expression| self.include_target(expression, scope));
                let event = self.direct_event(node, scope);
                let definite = event > 0
                    && matches!(
                        node.kind(),
                        "require_expression" | "require_once_expression"
                    );
                if definite {
                    self.store.php_require(
                        &scope.owner,
                        self.file,
                        event,
                        target.as_deref().unwrap_or(""),
                        line,
                    )?;
                }
                if let Some(target) = target {
                    self.store.reference(
                        &scope.owner,
                        "file",
                        &target,
                        None,
                        if definite {
                            "includes:definite"
                        } else {
                            "includes"
                        },
                        self.file,
                        line,
                    )?;
                } else {
                    // A bare relative include depends on runtime cwd/include_path.
                    // Preserve uncertainty for an active autoload file's
                    // transitive include chain without publishing a fake edge.
                    self.loader_unknown(scope)?;
                    self.stats.dynamic_php_references += 1;
                }
            }
            "eval_intrinsic" => {
                self.loader_unknown(scope)?;
            }
            _ => {}
        }
        Ok(true)
    }
}

pub fn extract(
    source: &[u8],
    file: &str,
    file_id: &str,
    store: &Store,
    stats: &mut Stats,
) -> Result<()> {
    let encoded = encode_source(source);
    let source = encoded.as_bytes();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_php::LANGUAGE_PHP.into())?;
    let tree = parser.parse(source, None).context("PHP parser cancelled")?;
    if tree.root_node().has_error() {
        bail!("PHP syntax error or unsupported grammar; refusing partial extraction");
    }
    let mut ex = Extractor {
        source,
        file,
        store,
        stats,
        computed_paths_allowed: !has_ticks(tree.root_node(), source),
    };
    let mut section = Vec::new();
    let base = Scope {
        owner: file_id.into(),
        activation_eligible: !has_jump(&children(tree.root_node())),
        activation_reachable: true,
        ..Default::default()
    };
    let mut scope = base.clone();
    for node in children(tree.root_node()) {
        if node.kind() == "namespace_definition" {
            ex.section(&section, scope, 0)?;
            section.clear();
            scope = base.clone();
            scope.namespace = node
                .child_by_field_name("name")
                .map(|n| ex.text(n).to_owned())
                .unwrap_or_default();
            if let Some(body) = node.child_by_field_name("body") {
                ex.section(&children(body), scope.clone(), 0)?;
                scope = base.clone();
            }
        } else {
            section.push(node);
        }
    }
    ex.section(&section, scope, 0)?;
    // Only literal HTML spans survive; PHP strings/comments must not become HTML links.
    // '<' ensures a PHP fragment inside a quoted HTML URL cannot turn into a
    // plausible static path after masking. Outside attributes it is harmless text.
    let mut masked: Option<Vec<u8>> = None;
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "text"
            && !source[node.byte_range()]
                .iter()
                .all(u8::is_ascii_whitespace)
        {
            masked.get_or_insert_with(|| vec![b'<'; source.len()])[node.byte_range()]
                .copy_from_slice(&source[node.byte_range()]);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return match masked {
                    Some(masked) => {
                        let restored = decode_source(std::str::from_utf8(&masked)?);
                        html::extract(&restored, file, file_id, store, ex.stats)
                    }
                    None => Ok(()),
                };
            }
        }
    }
}
