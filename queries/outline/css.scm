(rule_set (selectors) @name) @selector
(media_statement "@media" @name . (_) @name) @at_rule
(keyframes_statement "@keyframes" @name (keyframes_name) @name) @at_rule
(supports_statement "@supports" @name . (_) @name) @at_rule
