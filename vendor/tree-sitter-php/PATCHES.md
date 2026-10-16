# Local grammar fixes

Based on crates.io tree-sitter-php **0.24.2**, upstream
https://github.com/tree-sitter/tree-sitter-php. Original MIT LICENSE is retained.
`grammar.patch` records all changes from that release.

Changes in `common/define-grammar.js`:

1. `exit_statement` accepts an optional expression (including parenthesized
   expressions), as well as empty `exit()`. The original rule accepted only
   parentheses, rejecting valid `exit -1;` and `exit -callee();`.
2. `_simple_string_member_access_expression` uses `reserved('nothing', $.name)`
   for property names. This accepts keyword properties such as `"$item->class"`
   without swallowing the following escape or interpolated variable.
3. A distinct `short_echo_statement` represents `<?=` and its comma-separated
   expression list, both at the beginning of a file and in mixed HTML/PHP.
   Ordinary expression statements still accept only a single expression.
4. Class constant elements accept `TRUE`/`FALSE` as names, despite their lexical
   classification as Boolean literals in other expression contexts.
5. A promoted parameter can start with `readonly` without an explicit visibility
   modifier. It must still have either visibility or readonly.
6. Grouped namespace imports accept a trailing comma, including mixed and
   function groups.
7. Cast expressions accept a match expression as their operand.

No ignored ERROR nodes or relaxed general error handling. PHP identifier byte
encoding is handled separately in `src/php.rs`: invalid UTF-8 bytes become
U+F100 followed by U+E000 + byte, while a literal U+F100 is doubled. The encoding
is injective, preserves normal UTF-8 and line numbers, and is also used for
Composer keys. Non-UTF-8 symbols have their encoded spellings in graph labels;
byte offsets used in entity IDs refer to the encoded source. Include path string
literals, literal HTML spans, and generated Composer path literals are decoded
back to their original spelling. __DIR__/__FILE__ remain raw filesystem paths;
non-UTF-8 filesystem paths are not converted into invented Unicode paths.

Both variants' generated parser.c, grammar.json and node-types.json are shipped
so building the extractor requires neither Node nor the generator.

Regenerate using **tree-sitter-cli 0.25.10** (Node available on PATH):

```sh
(cd php && tree-sitter generate)
(cd php_only && tree-sitter generate)
```

Extractor regressions are in `tests/extraction.rs` and
`tests/parser_regressions.rs`. The vendor-study PHP lint audit confirmed all
repaired corpus PHP files are valid PHP 8.3.12; the separate `<?hh` fixtures are
inventoried transparently as unsupported Hack, without PHP symbol extraction.
