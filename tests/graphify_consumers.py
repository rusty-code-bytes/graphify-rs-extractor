"""Official Graphify consumer regression gate; requires its installed dependencies.

python tests/graphify_consumers.py --upstream /path/to/graphify --binary target/release/graphify-extractor
No project PHP execution, LLM calls or remote services. Fails if an expected
located dependency disappears, a wrong target appears, or contexts collapse.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('--upstream', type=Path, required=True)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    sys.path.insert(0, str(args.upstream.resolve()))
    from graphify.serve import _read_graph, _query_graph_text, _shortest_path_text
    from graphify.affected import affected_nodes
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix='gre-consumer-') as scratch:
        root = Path(scratch)
        sources = {
            'model.php': '<?php\nclass Item {}\nclass Base { public function act() {} }\nclass Child extends Base { public function act() {} }\nclass Decoy { public function act() {} }\n',
            'functions.php': '<?php\nfunction target() {}\n',
            'bootstrap.php': "<?php\nrequire __DIR__ . '/functions.php';\n",
            'client.php': "<?php\n\nfunction caller(Base $x) { $x->act(); target(); }\nfunction convert(Item $item): Item { return $item; }\n",
        }
        for name, source in sources.items():
            (root / name).write_text(source)
        subprocess.run([str(binary), 'extract', str(root), '--export-profile', 'graphify', '--workers', '2'], check=True, capture_output=True)
        graph = _read_graph(str(root / 'graphify-out/graph.json'))
        def node(label=None, qualified=None, file=None, line=None):
            matches = [nid for nid, n in graph.nodes(data=True)
                       if (label is None or n.get('label') == label)
                       and (qualified is None or n.get('qualified_name') == qualified)
                       and (file is None or n.get('source_file') == file)
                       and (line is None or n.get('source_location') == line)]
            assert len(matches) == 1, (label, qualified, file, line, matches)
            return matches[0]
        caller = node('caller', file='client.php', line='L3')
        base = node('Base')
        child_method = node(qualified='Child::act')
        decoy_method = node(qualified='Decoy::act')
        base_hits = {h.node_id for h in affected_nodes(graph, base)}
        assert caller in base_hits, 'class impact misses method caller'
        assert decoy_method not in base_hits, 'unrelated same-name method included'
        override_hits = {h.node_id for h in affected_nodes(graph, child_method)}
        assert caller in override_hits, 'override impact misses base-bound caller'
        assert decoy_method not in override_hits
        include_hits = {h.node_id for h in affected_nodes(graph, node('functions.php'))}
        assert node('bootstrap.php') in include_hits, 'requires omitted by affected defaults'
        path = _shortest_path_text(graph, {'source': caller, 'target': node('functions.php')})
        assert 'Shortest path (2 hops)' in path, path
        assert 'calls' in path and 'contains' in path
        texts = {'path': path}
        for context in ('parameter_type', 'return_type'):
            text = _query_graph_text(graph, 'convert', seeds=[node('convert')], context_filters=[context], depth=1, token_budget=10000)
            assert 'NODE Item [src=model.php loc=L2 ' in text, text
            assert 'context=' + context in text, text
            assert 'at=client.php:L4' in text, text
            assert 'NODE Decoy' not in text, text
            texts[context] = text
        assert graph.is_multigraph(), 'parallel reference contexts require multigraph'
        assert len([e for s,t,e in graph.edges(data=True) if s==node('convert') and t==node('Item') and e.get('relation')=='references']) == 2
        assert all(s in graph and t in graph for s,t in graph.edges())
        report = {'passed': True, 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'upstream': str(args.upstream.resolve()), 'checks': ['class impact', 'override impact', 'unrelated method negatives', 'include impact', 'directed containment path', 'parameter context', 'return context', 'parallel context preservation', 'provenance', 'declared endpoints'], 'query_text': texts}
        # A separate corpus prevents its unknown loader from changing the
        # independent legacy consumer cases above.
        activation = root / 'activation'
        activation.mkdir()
        activation_sources = {
            'composer.json': json.dumps({'autoload': {'files': ['loader.php']}}),
            'loader.php': "<?php\nrequire __DIR__ . '/hidden.php';\n",
            'hidden.php': '<?php\nnamespace App; function application_helper() {}\n',
            'library.php': '<?php\nfunction application_helper() {}\n',
            'decoy.php': '<?php\nnamespace Other; function application_helper() {}\n',
            'exact.php': "<?php\nnamespace App;\nrequire __DIR__ . '/library.php';\n\\application_helper();\n",
            'imported.php': "<?php\nnamespace App;\nuse function \\application_helper as imported;\nrequire __DIR__ . '/library.php';\nimported();\n",
            'uncertain.php': "<?php\nnamespace App;\nrequire __DIR__ . '/library.php';\napplication_helper();\n",
        }
        for name, source in activation_sources.items():
            (activation / name).write_text(source)
        subprocess.run([str(binary), 'extract', str(activation), '--export-profile', 'graphify',
                        '--workers', '2', '--exclude-file', 'hidden.php'], check=True, capture_output=True)
        graph = _read_graph(str(activation / 'graphify-out/graph.json'))
        for filename, line in [('exact.php', 'L4'), ('imported.php', 'L5')]:
            text = _query_graph_text(graph, filename, seeds=[node(filename)],
                                     context_filters=['call'], depth=1, token_budget=10000)
            assert 'NODE application_helper [src=library.php loc=L2 ' in text, text
            assert 'at=' + filename + ':' + line in text, text
            assert 'src=decoy.php' not in text and 'src=hidden.php' not in text, text
            report['query_text'][filename] = text
        text = _query_graph_text(graph, 'uncertain.php', seeds=[node('uncertain.php')],
                                 context_filters=['call'], depth=1, token_budget=10000)
        assert 'src=library.php' not in text and 'src=decoy.php' not in text, text
        report['query_text']['namespace_fallback_negative'] = text
        report['checks'].extend(['required exact function', 'required imported function',
                                 'require call provenance', 'unknown namespace fallback negative'])
        sql_root = root / 'mysql'
        sql_root.mkdir()
        (sql_root / 'schema.sql').write_text(
            'CREATE TABLE real_table(id INT);\n'
            'CREATE TABLE decoy(id INT);\n'
            'CREATE VIEW result_view AS\n'
            'SELECT id MOD 2 AS n FROM real_table;\n'
            'DELIMITER //\n'
            'CREATE PROCEDURE p() BEGIN SELECT id FROM real_table; END//\n'
            'DELIMITER ;\n')
        subprocess.run([str(binary), 'extract', str(sql_root), '--sql-dialect', 'mysql',
                        '--export-profile', 'graphify'], check=True, capture_output=True)
        graph = _read_graph(str(sql_root / 'graphify-out/graph.json'))
        text = _query_graph_text(graph, 'result_view', seeds=[node('result_view')],
                                 depth=1, token_budget=10000)
        assert 'NODE real_table [src=schema.sql loc=L1 ' in text, text
        assert 'NODE decoy' not in text, text
        assert 'at=schema.sql:L4' in text, text
        statements = [n for _, n in graph.nodes(data=True) if n.get('type') == 'sql_statement']
        assert len(statements) == 1 and statements[0]['extraction_coverage'] == 'statement_only'
        assert statements[0]['source_location'] == 'L6'
        report['query_text']['mysql_view'] = text
        report['checks'].extend(['mysql expression dependencies', 'mysql statement coverage metadata'])
        if args.out:
            args.out.parent.mkdir(parents=True, exist_ok=True)
            args.out.write_text(json.dumps(report, indent=2)+'\n')
        print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
