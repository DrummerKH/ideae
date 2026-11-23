(class_declaration name: (_) @name) @class
(mixin_declaration name: (_) @name) @mixin
(enum_declaration name: (_) @name) @enum
(enum_entry name: (_) @name) @enum_entry
(extension_declaration name: (_) @name) @extension
(type_alias (type_identifier) @name) @type_alias
(function_declaration signature: (function_signature name: (_) @name)) @function
(method_declaration signature: (method_signature (function_signature name: (_) @name))) @method
(declaration (constructor_signature name: (_) @name)) @constructor
(static_final_declaration name: (_) @name) @const
(member_declaration (declaration (initialized_identifier_list (initialized_identifier (identifier) @name)))) @field
