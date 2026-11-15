; Minimal Groovy/Gradle highlights (the grammar crate ships none).

[(line_comment) (block_comment) (shebang)] @comment

[(string_literal) (string_fragment) (multiline_string_fragment) (character_literal)] @string
(escape_sequence) @string.escape
(string_interpolation) @embedded

[(decimal_integer_literal) (hex_integer_literal) (octal_integer_literal) (binary_integer_literal)
 (decimal_floating_point_literal) (hex_floating_point_literal)] @number

[(true) (false)] @boolean
(null_literal) @constant

[(this) (super)] @variable.special

(method_invocation name: (identifier) @function)
(juxt_function_call name: (identifier) @function)
(method_declaration name: (identifier) @function)
(function_definition name: (identifier) @function)

(class_declaration name: (identifier) @type)
(type_identifier) @type

(annotation name: (identifier) @attribute)
(marker_annotation name: (identifier) @attribute)

(map_item key: (identifier) @property)

[
  "abstract" "as" "assert" "break" "case" "catch" "class" "continue" "def" "default" "do"
  "else" "enum" "extends" "final" "finally" "for" "if" "implements" "import" "in"
  "instanceof" "interface" "new" "package" "private" "protected" "public" "return"
  "static" "switch" "synchronized" "throw" "throws" "try" "while"
] @keyword
