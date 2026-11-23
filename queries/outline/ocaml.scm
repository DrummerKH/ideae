(module_binding (module_name) @name) @module
(type_binding name: (_) @name) @type
(exception_definition (constructor_declaration (constructor_name) @name)) @exception
(compilation_unit (value_definition (let_binding pattern: (value_name) @name (parameter)) @function))
(structure (value_definition (let_binding pattern: (value_name) @name (parameter)) @function))
(compilation_unit (value_definition (let_binding pattern: (value_name) @name) @let))
(structure (value_definition (let_binding pattern: (value_name) @name) @let))
