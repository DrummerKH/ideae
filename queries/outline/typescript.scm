(class_declaration name: (_) @name) @class
(abstract_class_declaration name: (_) @name) @class
(class name: (_) @name) @class
(interface_declaration name: (_) @name) @interface
(type_alias_declaration name: (_) @name) @type_alias
(enum_declaration name: (_) @name) @enum
(enum_body name: (_) @enum_member @name)
(enum_assignment name: (_) @name) @enum_member
(internal_module name: (_) @name) @namespace
(module name: (_) @name) @module
(function_declaration name: (_) @name) @function
(generator_function_declaration name: (_) @name) @function
(function_signature name: (_) @name) @function
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression) (generator_function)]) @function
(class_body (method_definition name: (_) @name) @method)
(class_body (abstract_method_signature name: (_) @name) @method)
(class_body (public_field_definition name: (_) @name) @field)
(interface_body (method_signature name: (_) @name) @method)
(interface_body (property_signature name: (_) @name) @property)
(type_alias_declaration value: (object_type (property_signature name: (_) @name) @property))
(program (lexical_declaration (variable_declarator name: (identifier) @name) @variable))
(program (variable_declaration (variable_declarator name: (identifier) @name) @variable))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @variable)))
