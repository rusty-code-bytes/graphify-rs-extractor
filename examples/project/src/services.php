<?php
namespace Demo;

class Writer
{
    public static function save() {}
}

class Audit
{
    public static function save() {}
}

class Client
{
    public function send() {}
    public function flush() { $this->send(); }
}
