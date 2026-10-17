; test_loops.asm
; Nested loop with a function call inside the inner loop body. Verifies
; Jump/JumpIfFalse correctness across iterations AND that Call/Return
; correctly save/restore bp + captures without corrupting the outer
; loop's locals (i, j, sum), since bp is reused every outer/inner pass.
;
; for i in 0..3:
;   for j in 0..3:
;     sum += add_one(i*10 + j)
;
; i=0: add_one(0)+add_one(1)+add_one(2)    = 1+2+3    = 6
; i=1: add_one(10)+add_one(11)+add_one(12) = 11+12+13 = 36
; i=2: add_one(20)+add_one(21)+add_one(22) = 21+22+23 = 66
; total = 108
;
; Expected output: 108

push 0
store 0        ; i = 0
push 0
store 2        ; sum = 0

outer:
load 0
push 3
lt
jz outer_end

push 0
store 1        ; j = 0

inner:
load 1
push 3
lt
jz inner_end

load 0
push 10
mul
load 1
add
push @add_one
callv 1
load 2
add
store 2        ; sum += add_one(i*10 + j)

load 1
push 1
add
store 1        ; j += 1
jmp inner

inner_end:
load 0
push 1
add
store 0        ; i += 1
jmp outer

outer_end:
load 2
call emit_value 1
pop
halt

.function add_one 1 0
add_one:
store 0
load 0
push 1
add
ret

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
