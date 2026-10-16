"""Exercise the real upstream Graphify loaders and query engine; no stub implementations."""
import argparse
import json
import sys
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--upstream", type=Path, required=True)
parser.add_argument("--graph", type=Path, required=True)
parser.add_argument("--query", required=True)
args = parser.parse_args()
sys.path.insert(0, str(args.upstream.resolve()))

from graphify.paths import load_node_link_graph
from graphify.serve import _read_graph, _query_graph_text

raw = json.loads(args.graph.read_text())
for loader in (load_node_link_graph, _read_graph):
    graph = loader(str(args.graph))
    assert graph.number_of_nodes() == len(raw["nodes"])
    assert graph.number_of_edges() == len(raw["links"])
    assert graph.is_directed() and graph.is_multigraph()
    print(f"{loader.__name__}: {graph.number_of_nodes()} nodes, {graph.number_of_edges()} edges")
result = _query_graph_text(graph, args.query)
assert result and "No matching" not in result, result
print(result)
