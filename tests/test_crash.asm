; test_crash.asm
; The pattern a compiler can generate for an explicit unwrap():
;   Dup
;   IsType(Error)
;   jz ok
;   Crash "division by zero (unwrap)"
; ok:
;
; Expected result: execution does NOT reach the last print,
; exits with a nonzero status and writes a message to stderr
; such as: "Runtime error: crash: division by zero (unwrap): Error: division by zero"

push 10
call emit_value 1            ; 10, showing that execution reaches the code before the error
pop

push 1
push 0
div              ; -> Value::Error(DivisionByZero), recoverable, on the stack

dup
is_type error
jz ok
crash "division by zero (unwrap)"
ok:

call emit_value 1            ; must not be reached
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
