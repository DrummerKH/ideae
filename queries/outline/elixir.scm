(call target: (identifier) @_k (arguments . (alias) @name) (#any-of? @_k "defmodule" "defprotocol" "defimpl")) @module
(call target: (identifier) @_k (arguments . (call target: (identifier) @name)) (#any-of? @_k "def" "defp" "defmacro" "defmacrop" "defguard" "defguardp" "defdelegate")) @function
(call target: (identifier) @_k (arguments . (identifier) @name) (#any-of? @_k "def" "defp" "defmacro" "defmacrop")) @function
(call target: (identifier) @_k (arguments . (binary_operator left: (call target: (identifier) @name))) (#any-of? @_k "def" "defp" "defmacro" "defmacrop")) @function
