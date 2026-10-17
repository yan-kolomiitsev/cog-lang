; test_logic.asm
; Checks And / Or / Xor / Not.
; Expected output, in order:
; true and false  -> false
; true or  false  -> true
; true xor true   -> false
; true xor false  -> true
; not true        -> false
; not false       -> true

push true
push false
and
call emit_value 1
pop

push true
push false
or
call emit_value 1
pop

push true
push true
xor
call emit_value 1
pop

push true
push false
xor
call emit_value 1
pop

push true
not
call emit_value 1
pop

push false
not
call emit_value 1
pop

; TypeMismatch check: and on a non-Bool must produce a recoverable Value::Error
push 1
push true
and
dup
is_type error
call emit_value 1   ; expect true (the result of is_type error)
pop
call emit_value 1   ; the Value::Error(...) underneath
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
