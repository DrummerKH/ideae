(class_declaration "interface" (type_identifier) @name) @interface
(class_declaration "enum" (type_identifier) @name) @enum
(class_declaration (type_identifier) @name) @class
(object_declaration (type_identifier) @name) @object
(infix_expression (object_literal) . (simple_identifier) @name . (lambda_literal)) @object
(companion_object (type_identifier) @name) @companion_object
(companion_object "companion" @name) @companion_object
(function_declaration (simple_identifier) @name) @function
(source_file (property_declaration (variable_declaration (simple_identifier) @name)) @property)
(class_body (property_declaration (variable_declaration (simple_identifier) @name)) @property)
(enum_entry (simple_identifier) @name) @enum_entry
(type_alias (type_identifier) @name) @type_alias
