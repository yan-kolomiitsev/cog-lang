; test_convert.asm
; Checks runtime conversions through CallNative.
; Expected output, in order:
; "42" to_int        -> 42
; 3.9 to_int          -> 3
; "3.14" to_float     -> 3.14
; 7 to_float          -> 7
; "true" to_bool      -> true
; "false" to_bool     -> false
; 42 to_int (identity)-> 42
; "truu" to_bool      -> true   (is_type error)
; "truu" to_bool      -> Error: cannot parse str as bool
; "abc" to_int        -> true   (is_type error)
; "abc" to_int        -> Error: cannot parse str as int
; true to_int (TypeMismatch) -> true   (is_type error)
; true to_int (TypeMismatch) -> Error: type mismatch in str::to_int

push "42"
call_native str::to_int
call emit_value 1
pop

push 3.9
call_native str::to_int
call emit_value 1
pop

push "3.14"
call_native str::to_float
call emit_value 1
pop

push 7
call_native str::to_float
call emit_value 1
pop

push "true"
call_native str::to_bool
call emit_value 1
pop

push "false"
call_native str::to_bool
call emit_value 1
pop

push 42
call_native str::to_int
call emit_value 1
pop

; recoverable ParseFailure: "truu" cannot be parsed as bool
push "truu"
call_native str::to_bool
dup
is_type error
call emit_value 1   ; true
pop
call emit_value 1   ; Error: cannot parse str as bool
pop

; recoverable ParseFailure: "abc" cannot be parsed as int
push "abc"
call_native str::to_int
dup
is_type error
call emit_value 1   ; true
pop
call emit_value 1   ; Error: cannot parse str as int
pop

; recoverable TypeMismatch: Bool -> Int is unsupported
push true
call_native str::to_int
dup
is_type error
call emit_value 1   ; true
pop
call emit_value 1   ; Error: type mismatch in str::to_int
pop

halt

; Ordinary output function built from generic runtime calls.
emit_value:
store 0
call_native io::stdout
load 0
call_native bytes::render
store 1
load 1
push 0
load 1
call_native bytes::len
call_native io::write_all
dup
is_type error
jnz emit_done
pop
call_native io::stdout
call_native io::flush
emit_done:
ret
