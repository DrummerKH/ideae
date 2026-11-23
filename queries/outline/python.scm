(class_definition name: (_) @name) @class
(function_definition name: (_) @name) @function
(module (expression_statement (assignment left: (identifier) @name)) @variable)
(class_definition body: (block (expression_statement (assignment left: (identifier) @name)) @field))
