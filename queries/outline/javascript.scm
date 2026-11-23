(class_declaration name: (_) @name) @class
(class name: (_) @name) @class
(function_declaration name: (_) @name) @function
(generator_function_declaration name: (_) @name) @function
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression) (generator_function)]) @function
(class_body (method_definition name: (_) @name) @method)
(class_body (field_definition property: (_) @name) @field)
(program (lexical_declaration (variable_declarator name: (identifier) @name) @variable))
(program (variable_declaration (variable_declarator name: (identifier) @name) @variable))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @variable)))
