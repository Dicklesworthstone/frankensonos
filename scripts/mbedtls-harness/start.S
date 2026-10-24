    .text
    .globl __start
    .set noreorder
    .ent __start
__start:
    lui   $gp, %hi(_gp)
    addiu $gp, $gp, %lo(_gp)
    move  $t0, $sp
    lw    $4, 0($t0)
    addiu $5, $t0, 4
    addiu $6, $4, 1
    sll   $6, $6, 2
    addu  $6, $6, $5
    lui   $7, %hi(main)
    addiu $7, $7, %lo(main)
    move  $8, $0
    move  $9, $0
    addiu $sp, $sp, -24
    sw    $t0, 16($sp)
    la    $t9, __uClibc_start_main
    jalr  $t9
    nop
    li    $4, 0
    li    $2, 4001
    syscall
    .end __start
