(preproc_def name: (_) @name) @macro
(preproc_function_def name: (_) @name) @macro
(type_definition declarator: (type_identifier) @name) @type
(struct_specifier name: (_) @name body: (_)) @struct
(enum_specifier name: (_) @name body: (_)) @enum
(union_specifier name: (_) @name body: (_)) @union
(enumerator name: (_) @name) @enumerator
(field_declaration declarator: (field_identifier) @name) @field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @field
(field_declaration declarator: (array_declarator declarator: (field_identifier) @name)) @field
(function_definition declarator: (function_declarator declarator: (_) @name)) @function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (_) @name))) @function
(translation_unit (declaration declarator: (function_declarator declarator: (_) @name)) @function)
(translation_unit (declaration declarator: (pointer_declarator declarator: (function_declarator declarator: (_) @name))) @function)
(translation_unit (declaration declarator: (init_declarator declarator: (identifier) @name)) @variable)
(translation_unit (declaration declarator: (identifier) @name) @variable)
