; test_stack.asm
; Checks for Dup / Pop / Swap.
; Expected output, in order:
; dup 5       -> 5, 5           (two consecutive print calls)
; pop         -> 1               (pop removes 2 from the top, leaving 1)
; swap        -> "a", "b"        (after swap, the print order is a then b)

push 5
dup
call emit_value 1   ; 5
pop
call emit_value 1   ; 5
pop

push 1
push 2
pop
call emit_value 1   ; 1
pop

push "a"
push "b"
swap
call emit_value 1   ; a  (b was on top; after swap it is a)
pop
call emit_value 1   ; b
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
