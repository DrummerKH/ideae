(module_definition name: (_) @name) @module
(struct_definition (type_head (identifier) @name)) @struct
(abstract_definition (type_head (identifier) @name)) @type
(function_definition (signature (call_expression . (identifier) @name))) @function
(assignment . (call_expression . (identifier) @name)) @function
(macro_definition (signature (call_expression . (identifier) @name))) @macro
(const_statement (assignment . (identifier) @name)) @const
