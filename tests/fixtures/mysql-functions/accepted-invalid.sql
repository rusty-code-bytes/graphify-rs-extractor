-- Regression fixture for a repaired parser validation gap: reject atomically.
-- MySQL comparison operators require an expression on both sides:
-- https://dev.mysql.com/doc/refman/8.4/en/comparison-operators.html#operator_equal-to
-- sqlparser 0.59 accepts AS as the right-hand identifier, then treats result
-- as an implicit alias. The missing right operand is therefore not rejected.
-- The extractor must retain the previous complete graph and identify this file.
CREATE TABLE known_source(i INT);
CREATE VIEW missing_comparison_operand AS
SELECT decoy.i <=> AS result
FROM known_source AS decoy;
