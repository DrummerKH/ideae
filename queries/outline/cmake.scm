(function_def (function_command (argument_list . (argument) @name))) @function
(macro_def (macro_command (argument_list . (argument) @name))) @macro
(normal_command (identifier) @_c (argument_list . (argument) @name) (#match? @_c "^(?i)(project|add_executable|add_library|add_custom_target)$")) @target
