(class_declaration declaration_kind: "struct" name: (_) @name) @struct
(class_declaration declaration_kind: "enum" name: (_) @name) @enum
(class_declaration declaration_kind: "extension" name: (_) @name) @extension
(class_declaration name: (_) @name) @class
(protocol_declaration name: (_) @name) @protocol
(protocol_function_declaration name: (simple_identifier) @name) @function
(protocol_property_declaration name: (pattern bound_identifier: (_) @name)) @property
(function_declaration name: (simple_identifier) @name) @function
(init_declaration "init" @name) @constructor
(source_file (property_declaration name: (pattern bound_identifier: (_) @name)) @property)
(class_body (property_declaration name: (pattern bound_identifier: (_) @name)) @property)
(enum_entry name: (simple_identifier) @enum_case @name)
(typealias_declaration name: (type_identifier) @name) @type_alias
