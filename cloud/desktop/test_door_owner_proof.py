"""The door proves the owner to amux (governance PR 3): /_door/check returns
X-Amux-Front-Door-Owner, the secret amux minted, for a password account with no
member token (whom amux treats as the host owner), never for a member, and
nothing when update.sh has not copied the secret yet.

  python3 -m unittest cloud/desktop/test_door_owner_proof.py
"""
import importlib.util, os, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def load_door(etc):
    os.environ["DOOR_USERS"] = os.path.join(etc, "users")
    os.environ["DOOR_MEMBERS"] = os.path.join(etc, "members")
    os.environ["DOOR_OWNER_PROOF"] = os.path.join(etc, "front-door-owner")
    spec = importlib.util.spec_from_file_location("door_under_test", os.path.join(HERE, "door.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class OwnerProof(unittest.TestCase):
    def setUp(self):
        self.etc = tempfile.mkdtemp()
        with open(os.path.join(self.etc, "users"), "w") as f:
            f.write("ethan scrypt$x$y\nmate scrypt$x$y\n")
        with open(os.path.join(self.etc, "members"), "w") as f:
            f.write("mate MEMBER-TOKEN\n")
        self.door = load_door(self.etc)

    def write_proof(self, value):
        with open(os.path.join(self.etc, "front-door-owner"), "w") as f:
            f.write(value + "\n")

    def test_the_owner_gets_the_proof_and_a_member_does_not(self):
        self.write_proof("SECRET-1")
        owner = dict(self.door.check_headers("ethan", None))
        self.assertEqual(owner.get("X-Amux-Front-Door-Owner"), "SECRET-1")
        self.assertEqual(owner.get("X-Door-User"), "ethan")
        mate = dict(self.door.check_headers("mate", self.door.members().get("mate")))
        self.assertEqual(mate.get("X-Amux-Member"), "MEMBER-TOKEN")
        self.assertNotIn("X-Amux-Front-Door-Owner", mate)

    def test_an_account_the_door_does_not_know_gets_no_proof(self):
        self.write_proof("SECRET-1")
        self.assertNotIn("X-Amux-Front-Door-Owner", dict(self.door.check_headers("stranger", None)))

    def test_no_secret_yet_means_no_header(self):
        self.assertNotIn("X-Amux-Front-Door-Owner", dict(self.door.check_headers("ethan", None)))


if __name__ == "__main__":
    unittest.main()
