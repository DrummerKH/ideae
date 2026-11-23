(module_declaration name: (_) @name) @module
(type_declaration name: (_) @name) @type
(union_variant name: (_) @name) @enum_member
(type_alias_declaration name: (_) @name) @type_alias
(value_declaration functionDeclarationLeft: (function_declaration_left . (lower_case_identifier) @name)) @function
