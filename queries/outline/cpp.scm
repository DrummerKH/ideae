(preproc_def name: (_) @name) @macro
(preproc_function_def name: (_) @name) @macro
(namespace_definition name: (_) @name) @namespace
(class_specifier name: (_) @name body: (_)) @class
(struct_specifier name: (_) @name body: (_)) @struct
(enum_specifier name: (_) @name body: (_)) @enum
(union_specifier name: (_) @name body: (_)) @union
(enumerator name: (_) @name) @enumerator
(type_definition declarator: (type_identifier) @name) @type
(alias_declaration name: (_) @name) @type_alias
(field_declaration declarator: (function_declarator declarator: (_) @name)) @method
(field_declaration declarator: (field_identifier) @name) @field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @field
(field_declaration declarator: (array_declarator declarator: (field_identifier) @name)) @field
(field_declaration_list (declaration declarator: (function_declarator declarator: (_) @name)) @constructor)
(function_definition declarator: (function_declarator declarator: (_) @name)) @function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (_) @name))) @function
(function_definition declarator: (reference_declarator (function_declarator declarator: (_) @name))) @function
(translation_unit (declaration declarator: (function_declarator declarator: (_) @name)) @function)
(declaration_list (declaration declarator: (function_declarator declarator: (_) @name)) @function)
(translation_unit (declaration declarator: (init_declarator declarator: (identifier) @name)) @variable)
(translation_unit (declaration declarator: (identifier) @name) @variable)
