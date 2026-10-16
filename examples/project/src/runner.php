<?php
namespace Demo\Web;

use Demo\Writer as Storage;
use Demo\Client;

require_once __DIR__ . '/services.php';

function run()
{
    Storage::save();
    \Demo\Audit::save();
}

function deliver(Client $client)
{
    $client->send();
}
?>
<a href="../index.html">Späť</a>
