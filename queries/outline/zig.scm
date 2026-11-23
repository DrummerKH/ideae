(variable_declaration (identifier) @name (struct_declaration)) @struct
(variable_declaration (identifier) @name (enum_declaration)) @enum
(variable_declaration (identifier) @name (union_declaration)) @union
(source_file (variable_declaration (identifier) @name) @const)
(struct_declaration (variable_declaration (identifier) @name) @const)
(function_declaration name: (_) @name) @function
(container_field name: (_) @name) @field
(test_declaration (string (string_content) @name)) @test
