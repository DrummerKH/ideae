(trait_definition name: (_) @name) @trait
(class_definition name: (_) @name) @class
(object_definition name: (_) @name) @object
(enum_definition name: (_) @name) @enum
(simple_enum_case name: (_) @name) @enum_case
(full_enum_case name: (_) @name) @enum_case
(type_definition name: (_) @name) @type
(function_definition name: (_) @name) @function
(function_declaration name: (_) @name) @function
(template_body (val_definition pattern: (identifier) @name) @field)
(template_body (var_definition pattern: (identifier) @name) @field)
(compilation_unit (val_definition pattern: (identifier) @name) @field)
(compilation_unit (var_definition pattern: (identifier) @name) @field)
